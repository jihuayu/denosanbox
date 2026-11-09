# Python examples

These examples require a source build of the binding in `bindings/python`:

```bash
cd bindings/python
python -m venv .venv
. .venv/bin/activate
python -m pip install "maturin>=1.7,<2.0"
maturin develop --release
cd ../..
python examples/python/main.py
python examples/python/deadline_and_cancel.py
python examples/python/modules_vfs.py
```

`main.py` prepares a two-module TypeScript graph with `prepare_modules`,
creates two independent sandboxes from it and demonstrates independent module
state, JSON calls and the explicit profile. The bare deno_core profile does not
expose `Deno.pid`; host PID verification is done by real async callbacks (see
`modules_vfs.py` and `bindings/python/tests/test_host_callbacks.py`).

`modules_vfs.py` shows the engine policy ceiling and per-sandbox request (private
VFS + `env` capability), a read-only `mount_directory` and a scoped async host
callback that reports the Python host PID.

`deadline_and_cancel.py` shows a runaway `while (true)` loop terminated by a
per-call deadline and by cancelling the Python task awaiting `Sandbox.call()`.
Both examples close sandboxes explicitly and shut the engine down through an
async context manager.

The examples use the default Deno/V8 backend. They do not work with a
`--no-default-features --features fake` contract-only build because that
backend does not execute JavaScript.
