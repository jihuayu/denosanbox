# denosanbox Python binding (experimental subset)

Native async Python bindings for the `denosanbox` Rust facade. The extension
module is `denosanbox._native`; the supported public API is the `denosanbox`
package.

Async host callbacks run on their captured asyncio loop through PyO3. A host
must declare capability names at Engine construction, register callbacks, then
explicitly grant each sandbox a trusted business scope. Default construction
grants no host capabilities. This binding is independently versioned 0.1.0
and uses the matching Rust facade; no wheels have been published.

## Supported platform

Initially tested only on:

- Linux x86_64
- CPython 3.12
- Rust toolchain pinned by the core repository

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
the Python host process. The bare deno_core profile does not expose `Deno.pid`; the real async callback tests verify Python host PID and event-loop thread identity.

## Not implemented

- Multi-module graph construction from Python. `prepare(source, entrypoint)`
  creates a one-module graph; the Rust `PluginSource` still supports multiple
  modules for other bindings.
- JSX and broad permission-policy helpers. Single-module TypeScript (`.ts` /
  `.mts`) uses the core offline transpiler; a multi-module Python builder is
  not exposed yet.
- A published wheel distribution and platforms beyond the tested matrix.

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

## Async host callbacks

Register callbacks while the owning asyncio loop is running. Callbacks receive
trusted metadata separately from guest JSON and return JSON text:

```python
async with denosanbox.Engine(host_capabilities=["orders.read"]) as engine:
    async def read_order(context, input_json):
        await asyncio.sleep(0.001)
        return json.dumps({"scope": context.scope, "pid": __import__("os").getpid()})

    engine.register_host("orders.read", read_order)
    plugin = engine.prepare("export async function run(input) { return await sandbox.host.call('orders.read', input); }")
    async with await engine.create(plugin, host_grants={"orders.read": "tenant-a"}) as sandbox:
        result = json.loads(await sandbox.call("run", '{"scope":"untrusted"}'))
        assert result["scope"] == "tenant-a"
```

`InvocationContext` exposes `sandbox_id`, `scope`, `remaining_ms`, and
`cancelled`. Guest-supplied IDs never replace these values. The callback body
runs on the registration loop and holds the GIL only while executing Python;
awaiting Rust execution does not block that loop. Registration alone gives no
sandbox a grant. Input/output byte limits are checked by Rust.

Callback cancellation is cooperative. The wrapper requests coroutine
cancellation when its sandbox closes or deadline expires; code that suppresses
`asyncio.CancelledError` may continue. Core retains its ownership/accounting and
reports incomplete cleanup until it finishes. Callback failures expose a
sanitized error rather than Python exception text. Timeouts do not roll back
already committed side effects. Close sandboxes and shut down engines before
stopping their callback event loops or finalizing the Python interpreter.

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
  engine.create(...)`) close/shutdown on exit. If core reports incomplete
  cleanup they raise `SandboxError(kind="incomplete_close")` with `report`,
  rather than discarding that diagnostic. Synchronous context managers are
  intentionally not provided because close/shutdown are async.

## Error kinds

`SandboxError` subclasses map one-to-one to stable Rust kinds:

`runtime_unavailable`, `unsupported_feature`, `unsupported_language`,
`invalid_plugin`, `module_not_found`, `module_denied`, `limit_exceeded`,
`queue_full`, `closed`, `cancelled`, `deadline_exceeded`, `execution`,
`backend`, `invalid_configuration`, `internal`, `policy`.

Unknown future kinds fall back to `SandboxError` while preserving `kind` and
`code`, so the binding remains forward-compatible.

## Tests

- `tests/test_fake_contract.py` exercises lifecycle, errors, deadlines,
  cancellation and context managers against the deterministic fake backend.
- `tests/test_deno_integration.py` exercises real JavaScript: two independent
  isolates, JSON round-trips, cancellation during creation, an infinite loop terminated by
  a deadline, cancellation on Python task cancellation, and deterministic
  close reports. These tests require the Deno backend and fail if it is unavailable.

`tests/test_host_callbacks.py` covers async loop/GIL dispatch, same-PID calls,
trusted scope, ungranted denial, callback cancellation, sanitized failures, and
explicit close before interpreter shutdown. Shared behavior fixtures also run
against Rust, Node, and the Go/C adapter.
