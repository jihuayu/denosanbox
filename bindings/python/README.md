# denosanbox Python binding (experimental subset)

Native async Python bindings for the `denosanbox` Rust facade. The extension
module is `denosanbox._native`; the supported public API is the `denosanbox`
package.

> **Status: experimental, incomplete subset.** This is not a complete release.
> Python host callback registration is **not implemented**, because the current
> Rust facade does not expose a callback registration API. Do not infer or add
> one from this binding. Release acceptance for callbacks (async callback
> invocation, GIL handling and object lifetime tests, issue #14) remains open
> until that Rust API exists and is tested end-to-end.

## Supported platform

Initially tested only on:

- Linux x86_64
- CPython 3.12
- Rust toolchain supported by the core facade (Rust 1.85+)

No other platforms or Python versions are claimed. No wheels are published and
no wheel platform/ABI support is claimed by this repository. Build from source
against the matching core checkout.

## What this binding does

- `Engine.prepare()` validates and freezes a single-module JavaScript ESM graph.
  The synchronous Rust call runs with the Python GIL released.
- `await Engine.create(prepared)` creates an independent isolate and module
  instance set.
- `await Sandbox.call(export, input_json, timeout_ms=None)` invokes an exported
  JavaScript function and returns the JSON-encoded result string.
- `Sandbox.cancel()` terminally cancels the current invocation and marks the
  sandbox closed.
- `await Sandbox.close()` requests close, joins the worker and returns a
  `CloseReport` reflecting actual completed/cancelled counts.
- `await Engine.shutdown()` closes every live sandbox and returns a
  `ShutdownReport`.
- Errors are translated into a stable `SandboxError` subclass hierarchy. Every
  exception exposes `kind` and `code` (both equal to the Rust `Error::kind()`
  string) plus a human-readable `message`.

There is no sidecar and no subprocess: the JavaScript runtime executes inside
the Python host process. The current bare deno_core profile does not expose `Deno.pid`. A guest-to-host callback PID assertion remains part of the unfinished callback release gate.

## Not implemented

- Python callbacks into JavaScript or JavaScript callbacks into Python. The
  facade has no registration method, so none is invented here.
- Multi-module graph construction from Python. `prepare(source, entrypoint)`
  creates a one-module graph; the Rust `PluginSource` still supports multiple
  modules for other bindings.
- TypeScript/JSX transpilation, permission policy helpers and a stable wheel
  distribution.

## Build and install from source

From `bindings/python` in the merged repository (the dependency expects the
facade crate at `../../core`):

```bash
python -m venv .venv
. .venv/bin/activate
python -m pip install "maturin>=1.7,<2.0" pytest pytest-asyncio
maturin develop --release
python -m pytest
```

For a contract-only build where the Deno backend source is not available yet,
build the explicit fake backend. These tests do **not** execute JavaScript:

```bash
maturin develop --no-default-features --features fake
python -m pytest tests/test_fake_contract.py
```

The production default is the real Deno/V8 backend; `Engine(backend="fake")`
is only for deterministic lifecycle/contract tests.

## API sketch

```python
import asyncio
import json

import denosanbox

JS = """
export async function echo(input) { return input; }

"""

async def main():
    async with denosanbox.Engine() as engine:
        prepared = engine.prepare(JS)
        a = await engine.create(prepared)
        b = await engine.create(prepared)

        assert json.loads(await a.call("echo", '{"ok": true}')) == {"ok": True}

        await a.close()
        await b.close()

asyncio.run(main())
```

JSON input is bounded in bytes before parsing and the GIL is released during parsing. JSON is passed and returned as `str` at the Python boundary, matching the
facade's `serde_json::Value` contract without exposing Rust serialization
types. `timeout_ms` overrides the engine/sandbox default for one call; the core
enforces a deadline and terminates runaway V8 execution.

## Cancellation and close semantics

- Cancellation is **terminal** for a sandbox. After `cancel()` or a deadline,
  later `call()` raises `ClosedError`.
- Cancelling the Python task awaiting `Sandbox.call()` calls
  `Sandbox.cancel()`. The Rust call future also owns a drop guard, so dropping
  the awaitable before completion requests cancellation.
- `Sandbox.close()` is idempotent and returns the completed report. Use it for
  deterministic worker join; relying on garbage collection is not sufficient.
- Engine creation is run as a detached Tokio task. If the Python awaitable is
  dropped mid-create, the task still completes and immediately drops the new
  handle, whose Rust `Drop` requests close. This avoids a permanently orphaned
  sandbox; it does not promise that cancellation returns synchronously.
- Async context managers (`async with Engine(...)`, `async with await
  engine.create(...)`) close/shutdown on exit. Synchronous context managers are
  intentionally not provided because close/shutdown are async.

## Error kinds

`SandboxError` subclasses map one-to-one to stable Rust kinds:

`runtime_unavailable`, `unsupported_feature`, `unsupported_language`,
`invalid_plugin`, `module_not_found`, `module_denied`, `limit_exceeded`,
`queue_full`, `closed`, `cancelled`, `deadline_exceeded`, `execution`,
`backend`, `invalid_configuration`, `internal`.

Unknown future kinds fall back to `SandboxError` while preserving `kind` and
`code`, so the binding remains forward-compatible.

## Tests

- `tests/test_fake_contract.py` exercises lifecycle, errors, deadlines,
  cancellation and context managers against the deterministic fake backend.
- `tests/test_deno_integration.py` exercises real JavaScript: two independent
  isolates, JSON round-trips, cancellation during creation, an infinite loop terminated by
  a deadline, cancellation on Python task cancellation, and deterministic
  close reports. These tests require the Deno backend and fail if it is unavailable.
