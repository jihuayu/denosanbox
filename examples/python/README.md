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
```

`main.py` creates two independent sandboxes from one prepared plugin and
demonstrates independent module state and JSON calls. The bare deno_core
profile does not expose `Deno.pid`; guest-to-host callback PID verification
remains part of the unfinished callback release gate.

`deadline_and_cancel.py` shows a runaway `while (true)` loop terminated by a
per-call deadline and by cancelling the Python task awaiting `Sandbox.call()`.
Both examples close sandboxes explicitly and shut the engine down through an
async context manager.

The examples use the default Deno/V8 backend. They do not work with a
`--no-default-features --features fake` contract-only build because that
backend does not execute JavaScript.
