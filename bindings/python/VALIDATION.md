# Python binding validation

Scope: implementation commit `4156653`, Linux x86_64, CPython 3.12,
Rust 1.99.1, `deno_core 0.413.0`, PyO3 0.23.5, pyo3-async-runtimes 0.23.0.

The final production-wheel rerun at this commit is **pending a fixture
correction** and is not recorded as passing here. Do not treat older counts or
pass results as the final result.

## Reproducible commands

Build and install the production wheel from a local artifact (nothing is
published):

```sh
cd bindings/python
python -m venv .venv
. .venv/bin/activate
python -m pip install "maturin>=1.7,<2.0" pytest pytest-asyncio
maturin build --locked --manifest-path bindings/python/Cargo.toml --out wheels
python -m pip install --force-reinstall wheels/*.whl
python -m pytest bindings/python/tests -q
python examples/python/main.py
python examples/python/deadline_and_cancel.py
python examples/python/modules_vfs.py
```

The generated development wheel tag is `cp312-cp312-manylinux_2_34_x86_64`
(a local build result, not a supported wheel-distribution matrix). Production
tests use the real embedded Deno/V8 backend and fail if it is unavailable; they
never fall back to fake execution.

Contract-only build (no JavaScript execution):

```sh
maturin develop --no-default-features --features fake
python -m pytest tests/test_fake_contract.py
```

## What the suite covers

- `tests/test_fake_contract.py`: deterministic lifecycle, errors, deadlines,
  cancellation and async context managers on the explicit fake backend.
- `tests/test_deno_integration.py`: two independent isolates with independent
  module state, JSON round-trips, infinite-loop deadlines, cancellation while
  awaiting a Python call, cancellation during sandbox initialization and
  deterministic close reports.
- `tests/test_modules_vfs.py`: multi-module/TypeScript/dynamic-import and
  managed guest Worker graphs, the explicit `pure-js-npm` profile (including the
  `node:worker_threads` alias that exists only there), private VFS and env under
  the engine ceiling plus sandbox request, `mount_directory` (read-only and
  read-write, no-follow), engine hard limits on queued/direct calls, zero
  timeout rejection and the cross-engine `PreparedPlugin` limit regression.
  The bounded memory probe allocates one 9 MiB `ArrayBuffer` under a 16 MiB heap
  target / 24 MiB tree budget and expects typed `limit_exceeded`; no unbounded
  or fatal heap probe is used because illegal native/process OOM is explicitly
  not contained.
- `tests/test_host_callbacks.py`: async callback dispatch on the registration
  asyncio loop with the Python host PID and trusted scope, ungranted denial,
  cancellation, sanitized failures and interpreter exit after explicit close.
  Non-cooperative work keeps core ownership/accounting until it finishes.
- `tests/test_parity.py`: the shared behavior fixtures, run with the same steps
  through the Rust and Node runners.

## Exposed surface and intentional exclusions

The binding exposes engine ceilings (`EngineOptions` with the policy `Limits`),
per-sandbox requests (`SandboxOptions`, `host_grants`), `mount_directory`, async
host callbacks and explicit `close()`/`shutdown()` reports. It does **not**
expose the advanced Rust `Engine::with_host_config` controls (capability
schemas, audit sink, shared host/memory pools), network/worker/delegation
helpers or a published wheel.

Worker, quota, narrowing and cleanup behavior is facade behavior: managed guest
Workers are a bounded JSON request/reply subset with no top-level pushes,
timers, streaming, `eval`/classic Workers, transferables/`SharedArrayBuffer`,
`sharedEnv` or native Workers; live heap, tree-memory and sandbox-count policy
changes are unsupported (recreate the sandbox).

## Last verified run (historical)

Before the final commit, the production wheel (rebuilt and reinstalled against
the then-current core) ran the complete suite successfully, including the four
host callback cases, the direct real-runtime cases, the shared behavior
fixtures and the deterministic contract cases, with no production test skipped.
Those counts predate later fixture and test changes and are **not** the final
result for `4156653`.
