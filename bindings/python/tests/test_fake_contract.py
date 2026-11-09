"""Contract tests against the deterministic `fake` backend.

These tests do not execute JavaScript. They exercise the Python-facing
lifecycle, error translation, cancellation and close semantics. Real V8/Deno
execution tests live in ``test_deno_integration.py``.
"""

from __future__ import annotations

import asyncio

import pytest

import denosanbox
from denosanbox import (
    CancelledError,
    ClosedError,
    DeadlineExceededError,
    Engine,
    ExecutionError,
    InvalidConfigurationError,
    InvalidPluginError,
    Sandbox,
    UnsupportedLanguageError,
)

ECHO_JS = """
export async function echo(input) { return input; }
"""


def test_package_version() -> None:
    assert denosanbox.__version__ == "0.1.0"


async def test_fake_echo_create_close_and_shutdown() -> None:
    engine = Engine("fake")
    assert engine.backend == "fake"
    prepared = engine.prepare(ECHO_JS)
    assert prepared.entrypoint == "main.js"
    assert prepared.module_count == 1
    assert prepared.total_bytes > 0

    sandbox = await engine.create(prepared)
    assert isinstance(sandbox, Sandbox)
    assert engine.active_sandboxes == 1
    assert await sandbox.call("echo", '{"hello": "world"}') == '{"hello":"world"}'

    report = await sandbox.close()
    assert isinstance(report, denosanbox.CloseReport)
    assert report.closed_cleanly is True
    assert isinstance(report.forced_termination, bool)
    assert report.invocations_completed >= 1
    assert report.invocations_cancelled >= 0

    # close is idempotent and returns the same completed report.
    assert await sandbox.close() is report

    shutdown = await engine.shutdown()
    assert isinstance(shutdown, denosanbox.ShutdownReport)
    # The engine registry can still count an explicitly closed handle until
    # the Python SandboxHandle wrapper is released; only cleanliness is stable.
    assert shutdown.sandboxes_closed >= 0
    assert shutdown.clean is True


async def test_prepared_plugin_is_reusable_and_sandboxes_are_independent() -> None:
    engine = Engine("fake")
    prepared = engine.prepare(ECHO_JS)

    a = await engine.create(prepared)
    b = await engine.create(prepared)
    assert engine.active_sandboxes == 2

    await a.call("set", '{"key": "k", "value": "a-only"}')
    assert await b.call("get", '{"key": "k"}') == "null"
    assert await a.call("get", '{"key": "k"}') == '"a-only"'

    assert await a.call("increment", '{"by": 2}') == "2"
    assert await b.call("increment", '{"by": 2}') == "2"

    await a.close()
    await b.close()
    assert engine.active_sandboxes == 0


async def test_async_context_managers_close_and_shutdown() -> None:
    async with Engine("fake") as engine:
        async with await engine.create(engine.prepare(ECHO_JS)) as sandbox:
            assert await sandbox.call("echo", "42") == "42"
        assert sandbox.closed is True
    # Engine.__aexit__ ran shutdown without raising.


async def test_execution_error_has_stable_kind_and_code() -> None:
    engine = Engine("fake")
    sandbox = await engine.create(engine.prepare(ECHO_JS))
    with pytest.raises(ExecutionError) as caught:
        await sandbox.call("boom", "null")
    err = caught.value
    assert err.kind == "execution"
    assert err.code == "execution"
    assert isinstance(err, denosanbox.SandboxError)
    assert err.message
    await sandbox.close()


async def test_invalid_json_input_maps_to_invalid_configuration() -> None:
    engine = Engine("fake")
    sandbox = await engine.create(engine.prepare(ECHO_JS))
    with pytest.raises(InvalidConfigurationError) as caught:
        await sandbox.call("echo", "{not-json")
    assert caught.value.kind == "invalid_configuration"
    assert caught.value.code == "invalid_configuration"
    await sandbox.close()


async def test_invalid_timeout_maps_to_invalid_configuration() -> None:
    engine = Engine("fake")
    sandbox = await engine.create(engine.prepare(ECHO_JS))
    with pytest.raises(InvalidConfigurationError):
        await sandbox.call("echo", "null", timeout_ms=0)
    await sandbox.close()


async def test_prepare_errors_are_translated() -> None:
    engine = Engine("fake")
    with pytest.raises(UnsupportedLanguageError) as caught:
        engine.prepare("export {};", "main.tsx")
    assert caught.value.kind == "unsupported_language"

    with pytest.raises(InvalidPluginError):
        engine.prepare("export {};", "main.txt")

    with pytest.raises(denosanbox.ModuleNotFoundSandboxError) as missing:
        engine.prepare("import './missing.js';", "main.js")
    assert missing.value.kind == "module_not_found"


async def test_deadline_on_blocking_fake_call_and_close() -> None:
    engine = Engine("fake")
    sandbox = await engine.create(engine.prepare(ECHO_JS))
    with pytest.raises(DeadlineExceededError) as caught:
        await sandbox.call("block", "null", timeout_ms=30)
    assert caught.value.kind == "deadline_exceeded"
    assert caught.value.code == "deadline_exceeded"

    report = await sandbox.close()
    assert isinstance(report, denosanbox.CloseReport)
    assert isinstance(report.closed_cleanly, bool)
    assert isinstance(report.forced_termination, bool)


async def test_cancelling_python_task_terminally_cancels_sandbox() -> None:
    engine = Engine("fake")
    sandbox = await engine.create(engine.prepare(ECHO_JS))

    task = asyncio.create_task(sandbox.call("block", "null", timeout_ms=60_000))
    await asyncio.sleep(0.1)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task

    assert sandbox.closed is True
    with pytest.raises(ClosedError):
        await sandbox.call("echo", "null")

    report = await sandbox.close()
    assert isinstance(report, denosanbox.CloseReport)


async def test_explicit_cancel_then_close() -> None:
    engine = Engine("fake")
    sandbox = await engine.create(engine.prepare(ECHO_JS))
    sandbox.cancel()
    with pytest.raises(ClosedError):
        await sandbox.call("echo", "null")
    report = await sandbox.close()
    assert isinstance(report, denosanbox.CloseReport)


async def test_default_engine_is_deno_and_may_be_unavailable_in_fake_build() -> None:
    try:
        engine = Engine()
    except denosanbox.RuntimeUnavailableError as exc:
        assert exc.kind == "runtime_unavailable"
        assert exc.code == "runtime_unavailable"
        return
    assert engine.backend == "deno"
    prepared = engine.prepare(ECHO_JS)
    try:
        sandbox = await engine.create(prepared)
    except denosanbox.RuntimeUnavailableError as exc:
        assert exc.kind == "runtime_unavailable"
        return
    await sandbox.close()
    await engine.shutdown()


async def test_create_requires_prepared_plugin() -> None:
    engine = Engine("fake")
    with pytest.raises(TypeError):
        await engine.create("not-prepared")  # type: ignore[arg-type]
    await engine.shutdown()


async def test_input_is_bounded_before_json_parsing() -> None:
    async with Engine("fake", max_message_bytes=16) as engine:
        async with await engine.create(engine.prepare(ECHO_JS)) as sandbox:
            # This would be invalid JSON too: the size limit must reject it first.
            with pytest.raises(denosanbox.LimitExceededError):
                await sandbox.call("echo", "x" * 17)
