#!/usr/bin/env python3
"""Validate portable conformance fixture JSON without executing guest code.

This is a structural validator only. It parses the fixture envelope, checks the
declared schema and source bounds, and asserts that fixture identities are
unique. It never imports, evaluates, or otherwise runs a fixture's JavaScript.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any

SCHEMA_VERSION = 1
EXPECTED_KIND = "denosanbox.conformance.fixtures"
MAX_JSON_BYTES = 2 * 1024 * 1024
MAX_DATA_DEPTH = 12
MAX_DATA_NODES = 100_000
MAX_FIELD_CHARS = 8192
ALLOWED_OUTCOMES = {
    "success",
    "module-load-denied",
    "operation-denied",
    "timeout",
}
ALLOWED_CATEGORIES = {
    "state-isolation",
    "module-import",
    "guest-api-denial",
    "deadline",
}
ALLOWED_FILESYSTEM = {"none", "read-only-sandbox"}
ID_RE = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
ESM_EXPORT_RE = re.compile(r"(?m)^\s*export\b")
ESM_IMPORT_RE = re.compile(r"(?m)\b(?:import|export)\b")


class ValidationError(ValueError):
    """Raised when the fixture document does not match the declared schema."""


def fail(path: str, message: str) -> None:
    raise ValidationError(f"{path}: {message}")


def expect_dict(value: Any, path: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(path, f"expected object, got {type(value).__name__}")
    return value


def expect_list(value: Any, path: str) -> list[Any]:
    if not isinstance(value, list):
        fail(path, f"expected array, got {type(value).__name__}")
    return value


def expect_str(value: Any, path: str, *, max_chars: int = MAX_FIELD_CHARS) -> str:
    if not isinstance(value, str):
        fail(path, f"expected string, got {type(value).__name__}")
    if not value.strip():
        fail(path, "must be a non-empty string")
    if len(value) > max_chars:
        fail(path, f"string is {len(value)} characters; limit is {max_chars}")
    if "\x00" in value:
        fail(path, "must not contain NUL")
    return value


def expect_bool(value: Any, path: str) -> bool:
    if not isinstance(value, bool):
        fail(path, f"expected boolean, got {type(value).__name__}")
    return value


def expect_int(value: Any, path: str, *, low: int, high: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        fail(path, f"expected integer, got {type(value).__name__}")
    if not low <= value <= high:
        fail(path, f"value {value} outside allowed range [{low}, {high}]")
    return value


def expect_string_list(value: Any, path: str, *, allow_empty: bool) -> list[str]:
    items = expect_list(value, path)
    if not allow_empty and not items:
        fail(path, "must contain at least one item")
    seen: set[str] = set()
    result: list[str] = []
    for index, item in enumerate(items):
        text = expect_str(item, f"{path}[{index}]")
        if text.casefold() in seen:
            fail(f"{path}[{index}]", f"duplicate item {text!r}")
        seen.add(text.casefold())
        result.append(text)
    return result


def check_bounded_data(value: Any, path: str, *, depth: int = 0) -> int:
    """Check JSON-safe bounded shape and return a node count."""
    if depth > MAX_DATA_DEPTH:
        fail(path, f"nesting exceeds {MAX_DATA_DEPTH} levels")
    if isinstance(value, dict):
        nodes = 1
        for key, child in value.items():
            if not isinstance(key, str):
                fail(path, f"non-string object key {key!r}")
            if len(key) > 256:
                fail(f"{path}.<key>", "object key exceeds 256 characters")
            nodes += check_bounded_data(child, f"{path}.{key}", depth=depth + 1)
            if nodes > MAX_DATA_NODES:
                fail(path, f"data shape exceeds {MAX_DATA_NODES} nodes")
        return nodes
    if isinstance(value, list):
        if len(value) > 10_000:
            fail(path, "array exceeds 10000 items")
        nodes = 1
        for index, child in enumerate(value):
            nodes += check_bounded_data(child, f"{path}[{index}]", depth=depth + 1)
            if nodes > MAX_DATA_NODES:
                fail(path, f"data shape exceeds {MAX_DATA_NODES} nodes")
        return nodes
    if isinstance(value, str):
        if len(value) > MAX_FIELD_CHARS:
            fail(path, f"string is {len(value)} characters; limit is {MAX_FIELD_CHARS}")
        return 1
    if value is None or isinstance(value, (bool, int, float)):
        return 1
    fail(path, f"non-JSON value of type {type(value).__name__}")
    return 0


def validate_requirements(raw: Any, path: str) -> None:
    requirements = expect_dict(raw, path)
    features = expect_string_list(
        requirements.get("features"), f"{path}.features", allow_empty=False
    )
    if "in-process-isolate" not in features:
        fail(f"{path}.features", "must include 'in-process-isolate'")
    expect_string_list(
        requirements.get("permissions"), f"{path}.permissions", allow_empty=True
    )
    expect_bool(requirements.get("network"), f"{path}.network")
    filesystem = expect_str(requirements.get("filesystem"), f"{path}.filesystem", max_chars=64)
    if filesystem not in ALLOWED_FILESYSTEM:
        fail(f"{path}.filesystem", f"unknown value {filesystem!r}")
    limits = expect_dict(requirements.get("limits"), f"{path}.limits")
    expect_int(limits.get("deadlineMs"), f"{path}.limits.deadlineMs", low=1, high=600_000)
    if "cpuBound" in limits:
        expect_bool(limits["cpuBound"], f"{path}.limits.cpuBound")


def validate_expected(raw: Any, path: str) -> None:
    expected = expect_dict(raw, path)
    outcome = expect_str(expected.get("outcome"), f"{path}.outcome", max_chars=64)
    if outcome not in ALLOWED_OUTCOMES:
        fail(f"{path}.outcome", f"unknown outcome {outcome!r}")
    expect_string_list(
        expected.get("procedure"), f"{path}.procedure", allow_empty=False
    )
    observations = expect_dict(expected.get("observations"), f"{path}.observations")
    if not observations:
        fail(f"{path}.observations", "must not be empty")
    expect_string_list(
        expected.get("assertions"), f"{path}.assertions", allow_empty=False
    )


def validate_case(raw: Any, index: int, *, max_source_bytes: int) -> str:
    path = f"cases[{index}]"
    case = expect_dict(raw, path)

    fixture_id = expect_str(case.get("id"), f"{path}.id", max_chars=128)
    if not ID_RE.fullmatch(fixture_id):
        fail(
            f"{path}.id",
            "must be lowercase kebab-case (letters, digits, single hyphens)",
        )
    expect_str(case.get("name"), f"{path}.name", max_chars=256)

    category = expect_str(case.get("category"), f"{path}.category", max_chars=64)
    if category not in ALLOWED_CATEGORIES:
        fail(f"{path}.category", f"unknown category {category!r}")

    source = expect_str(case.get("source"), f"{path}.source")
    source_bytes = len(source.encode("utf-8"))
    if source_bytes > max_source_bytes:
        fail(
            f"{path}.source",
            f"source is {source_bytes} UTF-8 bytes; limit is {max_source_bytes}",
        )
    if not ESM_EXPORT_RE.search(source):
        fail(f"{path}.source", "must be an ESM module with a top-level export")
    if not ESM_IMPORT_RE.search(source):
        fail(f"{path}.source", "must be an ESM module")

    validate_requirements(case.get("requirements"), f"{path}.requirements")
    validate_expected(case.get("expected"), f"{path}.expected")
    check_bounded_data(case, path)
    return fixture_id


def validate_document(document: Any, *, file_size: int) -> list[str]:
    root = expect_dict(document, "$")
    if file_size > MAX_JSON_BYTES:
        fail("$", f"file is {file_size} bytes; limit is {MAX_JSON_BYTES}")

    version = expect_int(
        root.get("schemaVersion"), "$.schemaVersion", low=SCHEMA_VERSION, high=SCHEMA_VERSION
    )
    if version != SCHEMA_VERSION:
        fail("$.schemaVersion", f"expected {SCHEMA_VERSION}")

    kind = expect_str(root.get("kind"), "$.kind", max_chars=128)
    if kind != EXPECTED_KIND:
        fail("$.kind", f"expected {EXPECTED_KIND!r}")

    status = expect_str(root.get("status"), "$.status", max_chars=64)
    if status != "declarative-only":
        fail("$.status", "must be 'declarative-only' until runtime execution is wired")

    expect_str(root.get("description"), "$.description", max_chars=2048)
    expect_str(root.get("executionNote"), "$.executionNote", max_chars=2048)

    limits = expect_dict(root.get("sourceLimits"), "$.sourceLimits")
    max_source_bytes = expect_int(
        limits.get("maxSourceBytes"),
        "$.sourceLimits.maxSourceBytes",
        low=1,
        high=1_048_576,
    )

    cases = expect_list(root.get("cases"), "$.cases")
    if not cases:
        fail("$.cases", "must contain at least one fixture")

    ids: list[str] = []
    names: list[str] = []
    for index, raw_case in enumerate(cases):
        fixture_id = validate_case(raw_case, index, max_source_bytes=max_source_bytes)
        ids.append(fixture_id)
        names.append(expect_str(raw_case["name"], f"cases[{index}].name", max_chars=256))

    seen_ids: dict[str, int] = {}
    for index, fixture_id in enumerate(ids):
        key = fixture_id.casefold()
        if key in seen_ids:
            fail(
                f"cases[{index}].id",
                f"duplicate fixture id {fixture_id!r} (first at cases[{seen_ids[key]}])",
            )
        seen_ids[key] = index

    seen_names: dict[str, int] = {}
    for index, name in enumerate(names):
        key = name.casefold()
        if key in seen_names:
            fail(
                f"cases[{index}].name",
                f"duplicate fixture name {name!r} (first at cases[{seen_names[key]}])",
            )
        seen_names[key] = index

    check_bounded_data(root, "$")
    return ids


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Validate conformance fixture JSON shape without executing it."
    )
    parser.add_argument(
        "path",
        nargs="?",
        type=Path,
        default=Path(__file__).resolve().parent.parent / "fixtures" / "conformance" / "cases.json",
        help="fixture envelope path (default: fixtures/conformance/cases.json)",
    )
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="only print the final summary or errors",
    )
    args = parser.parse_args(argv)

    try:
        raw_bytes = args.path.read_bytes()
    except OSError as error:
        print(f"error: cannot read {args.path}: {error}", file=sys.stderr)
        return 2

    try:
        document = json.loads(raw_bytes.decode("utf-8"))
    except UnicodeDecodeError as error:
        print(f"error: {args.path} is not valid UTF-8: {error}", file=sys.stderr)
        return 2
    except json.JSONDecodeError as error:
        print(f"error: {args.path} is not valid JSON: {error}", file=sys.stderr)
        return 2

    try:
        ids = validate_document(document, file_size=len(raw_bytes))
    except ValidationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    if not args.quiet:
        for fixture_id in ids:
            print(f"ok {fixture_id}")
        print()
    print(
        f"Validated {len(ids)} fixture(s) in {args.path} "
        f"({len(raw_bytes)} bytes). Schema/static checks only; "
        "fixture JavaScript was not executed."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
