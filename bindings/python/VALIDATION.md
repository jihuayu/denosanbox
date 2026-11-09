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
