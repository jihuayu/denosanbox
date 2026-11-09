# denosanbox experimental C ABI

This crate is a direct native adapter over the `denosanbox` Rust facade.
It does not implement JavaScript, a second runtime, a subprocess/sidecar, guest
callbacks or a policy engine. Every sandbox operation goes through the real
Deno-backed core.

## Status

- ABI version: **1** (`deno_abi_version()`), independently versioned package
  `0.1.0`.
- Stable shape for this experimental subset, not a final release surface.
- Completion is **polling only**. The core callback registration parity work and
  the release gate are still open, so this binding deliberately exposes no C
  callback API yet. It does not claim issue-16 completeness.
- Tested only on **Linux x86_64**. No other platform claim is made.
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

- Opaque owned `u64` tokens for engine, sandbox, job and buffer values. Tokens
  are kind-checked and generated from one monotonic, non-reused registry.
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

See `include/denosanbox.h` for the exact version-1 declarations and lifetime
rules. The C example is `examples/c/two_sandboxes.c`; edge coverage is
`examples/c/abi_edge.c`.

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

Run C ownership/edge examples, Go context/race tests, and shared behavior parity:

```sh
sh bindings/c/test.sh
```
