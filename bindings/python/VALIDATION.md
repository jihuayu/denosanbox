# Local validation — 2026-11-09

Environment: Linux x86_64, CPython 3.12.14, Rust 1.99.1,
deno_core 0.413.0, PyO3 0.23.5, pyo3-async-runtimes 0.23.0.

The production wheel was built with `maturin build`, installed into an isolated
virtual environment using `pip install --force-reinstall`, then tested:

```text
python -m pytest bindings/python/tests -q
19 passed in 0.69s
```

The suite includes 14 deterministic fake-backend contracts and 5 real Deno
integration cases. The latter cover independent module state, JSON round trips,
infinite-loop deadlines, cancellation while awaiting a Python call, async
context-manager close, and cancellation during sandbox initialization.
Production tests fail if the Deno backend is unavailable; they do not fall back
to fake execution.

`examples/python/main.py` and `examples/python/deadline_and_cancel.py` both ran
successfully against the installed production wheel. Cargo fmt and production
clippy (`-D warnings`) passed. A separate explicit fake-only wheel was built
and installed before production testing; its then-current 13 contract tests
passed. This does not establish production execution on its own.

The generated development wheel tag was
`cp312-cp312-manylinux_2_34_x86_64`. This is a local build result, not a supported
wheel-distribution matrix. No wheel was uploaded or published.

Async host callback/GIL/interpreter-shutdown parity is unimplemented and remains
a release gate for issue #14. No callback test result or guest PID verification
is claimed by this validation.

## Callback and shared fixture validation

Against committed Rust core d4855fe, the updated production wheel was rebuilt
and installed. The complete Python suite passed **28 tests in 0.96 s**, including
four host callback cases (asyncio-thread/GIL dispatch with host PID and trusted
scope, cancellation, sanitized failure, and interpreter exit after explicit
close), six direct real-runtime cases, four shared behavior fixtures, and
fourteen deterministic contract cases. No production test was skipped.

The earlier callback release-gate note above records the initial subset. Async
host registration and dispatch are now implemented, with explicit engine
ceilings and per-sandbox grants. Native and wrapper cancellation preserve
ownership; incomplete close reports are surfaced by async context managers.
The new callback tests run against the actual extension, not a mock callback.

## Multi-module / profile / private-VFS work (bindings #14/#15)

Changes in this pass (all under `bindings/python/`, `bindings/nodejs/`,
`examples/python/`, `examples/nodejs/`):

- `Engine.prepare_modules(modules, entrypoint='main.js', profile=None)` (Python)
  and `Engine.prepareModules(modules, entrypoint, profile)` (Node) forward a
  host-supplied graph and an explicit profile
  (`javascript-only` | `pure-js-npm`) to the real facade
  `Engine::prepare_with_profile(PluginSource, modules::CompatibilityProfile)`.
  In Node the validated profile travels with the immutable `PreparedPlugin`
  into `NativeEngine.create_modules(graph, entrypoint, profile, ...)`; unknown
  profile names are rejected, never downgraded.
  There is no compatibility fallback and no automatic JavaScript-only
  downgrade. `prepare()` is unchanged.
- `EngineOptions`/`SandboxOptions` (Python) and `EngineOptions`/create options
  (Node) map the facade policy ceiling (capabilities, read/write roots) and
  `Limits` (`heap_target_bytes`, `memory_budget_bytes`, `max_sandboxes`,
  `deadline`, `queue_depth`, `max_message_bytes`, `max_log_bytes`). Node and
  Python also expose the real facade `EngineLimits` (including
  `max_module_bytes`/`max_graph_bytes`/`max_modules`), enforced by the facade
  on every create. The ceiling is trusted host configuration that starts empty
  and may grant explicit capabilities/roots; per-sandbox requests are narrowed
  by the facade and exceeding the ceiling raises `policy`.
- `Engine.mount_directory(...)` / `Engine.mountDirectory(...)` wrap facade
  `SandboxHandle::mount_directory`.
- Close output shape: Python `close()` → `CloseReport` (now with
  `pending_host_operations`), `shutdown()` → `ShutdownReport`; Node `close()` →
  frozen `{closedCleanly, forcedTermination, invocationsCompleted,
  invocationsCancelled, pendingHostOperations}` and `shutdown()` → boolean
  `clean`. Non-clean reports are returned truthfully and never cached as
  success.
- New tests: `bindings/python/tests/test_modules_vfs.py`,
  `bindings/nodejs/tests/modules.test.cjs`. Fixtures follow the supported guest
  surface: dynamic modules export functions, the pure profile exports a
  callable `kind()`, workers use `new Worker('./worker.js', {type: 'module'})`
  with handlers attached before `postMessage` and an awaited `terminate()`,
  and every `call` passes exactly one JSON argument. The bounded memory probe
  allocates a single 9 MiB `ArrayBuffer` with a 16 MiB heap target / 24 MiB
  tree budget and expects the typed `limit_exceeded`; no unbounded/fatal heap
  probe is used because same-process OOM is explicitly not contained.
- Cross-Engine regression (Python and Node): a plugin prepared by engine A is
  passed to engine B with `maxModuleBytes=1`; `create` must reject with the
  exact typed `limit_exceeded`, and the plugin still works on A.
- Both committed `bindings/*/Cargo.lock` files were regenerated against the
  current manifests.

**Facade dependency:** the native bindings compile only once the runtime
worker's `Engine::prepare_with_profile` is integrated; the same checkpoint must
land the core `create` fix that enforces the creating engine's `EngineLimits`
for foreign `PreparedPlugin`s. No native build or runtime test was run in this
pass, so no new runtime test count is claimed; the last fully passing run is
recorded above.
