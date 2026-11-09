# denosanbox experimental C ABI

This crate is a direct native adapter over the `denosanbox` Rust facade. It does
not implement JavaScript, a second runtime, a subprocess/sidecar or a policy
engine. Every sandbox operation goes through the real embedded Deno/V8 core.

## Status

- ABI version: `deno_abi_version()`; read `include/denosanbox.h` for the exact
  current value and declarations. Package version `0.1.0`, independently
  versioned.
- Stable shape for this experimental subset, not a final release surface.
- Completion is **polling only** (`deno_job_wait`).
- Scoped native host callbacks are exposed: engine capability ceiling,
  registration with bounded input/output, per-sandbox grants, trusted callback
  context (`deno_context_json`, `deno_context_check`).
- Tested only on **Linux x86_64** with Rust 1.99.1 / `deno_core 0.413.0`. No
  other platform claim is made.
- Recoverable Rust panics are contained with `catch_unwind` at every fallible
  extern boundary. Aborts, OOM and native faults are not contained.

## Build

```sh
cd bindings/c
cargo build
# target/debug/libdenosanbox_c.so
```

To reuse an existing core/V8 build (for example the Node binding's `target`):

```sh
CARGO_TARGET_DIR=../nodejs/target cargo build
```

The normal/default documentation path is `bindings/c/target/debug`. The Go
adapter links that path by default; see `bindings/go/README.md` when using a
shared target directory.

## API shape

- Opaque owned `u64` tokens for engine, sandbox, job, buffer, context and
  callback handles. Tokens are kind-checked and generated from one monotonic,
  non-reused registry.
- `deno_engine_new`, `deno_engine_create_sandbox` (source cap 8 MiB).
- `deno_sandbox_call_submit` (export cap 128 bytes, JSON input cap 1 MiB,
  optional timeout) and `deno_job_wait` polling.
- `deno_job_cancel` is terminal sandbox cancellation; `deno_job_free` only
  releases the owner token and is safe while the task still runs.
- `deno_sandbox_close` / `deno_sandbox_free`, then
  `deno_engine_close` / `deno_engine_free`.
- `deno_buffer` and `deno_error` own registry storage and must be freed by
  token. Freed structs are zeroed; copying a token and freeing the copy is a
  typed invalid handle, not a double free.
- `NULL + len == 0` is the empty input slice; `NULL + len > 0` is invalid. Cap
  checks happen before any Rust slice creation or JSON parse.
- All `out_*` and `error_out` pointers are required. The caller guarantees
  pointer validity/readability/writability; native pointer faults are outside
  the ABI's containment guarantee.

## Host callbacks

- `deno_engine_new_with_capabilities` fixes the engine's capability-name
  ceiling; `deno_engine_new` remains deny-all.
- `deno_engine_register_host(engine, name, ..., callback, release, user_data)`
  registers a callback (bounded input/output, 1..=64 MiB). Registration alone
  grants nothing; ownership of `user_data` transfers only on
  `DENO_STATUS_OK`.
- `deno_engine_create_sandbox_with_grants` grants scopes from trusted host JSON;
  guest source and guest JSON never contribute a grant.
- Callbacks run synchronously on a managed native host thread (never the
  isolate thread, never holding the registry lock), receive the borrowed
  `input` plus a `deno_context_token` whose trusted metadata comes from the
  invocation, and return an owned `deno_buffer` produced by `deno_buffer_copy`.
  Not-OK statuses become one generic typed policy error; foreign error text is
  never forwarded.
- `release(user_data)` is called exactly once by native code after the last
  registration, sandbox grant and in-flight invocation no longer references the
  callback. There are no deferred tombstones and no global-thread heuristic: a
  callback that is still running keeps its owner alive, and
  `deno_sandbox_pending_host_operations` reports work that outlived a close.
- Context tokens are borrowed; only the native owner releases them.

See `include/denosanbox.h` for the exact declarations and lifetime rules. The C
examples are `examples/c/two_sandboxes.c`, `examples/c/abi_edge.c` and
`examples/c/host_callbacks.c`.

## Example build/run

```sh
cc -std=c11 -Wall -Wextra -Ibindings/c/include \
  examples/c/two_sandboxes.c \
  -Lbindings/c/target/debug -ldenosanbox_c \
  -Wl,-rpath,"$PWD/bindings/c/target/debug" \
  -o /tmp/denosanbox-c-example
/tmp/denosanbox-c-example
```

No package is published from this experimental binding.

The ABI admits at most 132 in-flight submission tasks across engines and uses a
two-thread Tokio executor. A reservation is held until actual native work ends,
even after the client cancels/frees its job token; saturation returns the typed
limit status. Submission deadlines are pinned before scheduling. Completed
owned result/job handles remain the caller's responsibility to free.

Run the C ownership/edge/callback examples, Go context/race tests, and shared
behavior parity:

```sh
sh bindings/c/test.sh
```
