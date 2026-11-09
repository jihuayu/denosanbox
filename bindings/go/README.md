# denosanbox thin Go adapter (experimental)

This package is cgo-only glue over the C ABI in `bindings/c`. It contains no
JavaScript runtime, sandbox policy, job scheduler or subprocess logic. Calls and
cancellation are delegated to the embedded native core; async completion uses
`deno_job_wait` polling. Scoped host callbacks go through the C callback bridge.

Tested only on **Linux x86_64** with Go **1.27.3** and the C artifact built from
Rust 1.99.1 / `deno_core 0.413.0`. No other platform/version is claimed and no
package is published.

## Prerequisites

Build the C library first and keep it at the default path used by cgo:

```sh
cd bindings/c
cargo build
# bindings/c/target/debug/libdenosanbox_c.so
```

The package uses `${SRCDIR}`-relative cgo flags:

```c
#cgo CFLAGS: -I${SRCDIR}/../c/include
#cgo LDFLAGS: -L${SRCDIR}/../c/target/debug -ldenosanbox_c -Wl,-rpath,${SRCDIR}/../c/target/debug
```

If the C crate was built with a shared `CARGO_TARGET_DIR`, supply the library
directory explicitly instead of moving build outputs:

```sh
export CGO_LDFLAGS="-L$PWD/bindings/nodejs/target/debug"
export LD_LIBRARY_PATH="$PWD/bindings/nodejs/target/debug${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
```

No `go:generate` hook shells out to a runtime and normal calls never spawn a
sidecar. Production calls execute in this host process through the native core.

## Usage

```go
engine, err := denosanbox.New()
sandbox, err := engine.Create(source)
result, err := sandbox.Call(ctx, "exportName", json.RawMessage(`{"x":1}`), 5*time.Second)
report, err := sandbox.Close() // awaits native close, then frees the C token
shutdown, err := engine.Close()
```

`Sandbox.Call` submits one native job and polls in 50 ms bounded waits. Context
cancellation calls the core's terminal sandbox cancel and returns the context
error. Context cancellation preserves the deadline identity of the native call
(the call keeps the deadline it was admitted with, regardless of which side
finishes first), so a race between Go's context and native completion cannot
turn a deadline error into a cancellation or vice versa. A cancelled sandbox
cannot be reused; later calls report `StatusClosed` or `StatusCancelled`.

`Close` returns the core close/shutdown report. A report with `ClosedCleanly` or
`Clean` false is still returned as data (not silently rewritten); the typed error
is reserved for actual close/free failures.

## Host callbacks

Callbacks registered on the adapter (see `callbacks.go`) run through the C ABI's
native host callback bridge. They receive a `HostContext` with trusted metadata
(sandbox id, scope, remaining deadline as a `context.Context` deadline, and
cancellation) separately from guest JSON, and return bounded JSON. The callback
owner is released exactly once after the last registration, grant and in-flight
invocation are finished — there is no deferred tombstone cleanup and no
global-thread heuristic. A callback still executing when its sandbox closes
stays accounted for and is visible through `deno_sandbox_pending_host_operations`
and the close report.

## Test and example

```sh
cd bindings/go
go test -race ./...

cd ../../examples/go
go run .
```

`go run` here only launches the example program; the adapter itself never shells
out. Both sandboxes live in the example's host PID. No guest PID is claimed.

Limitations: Linux x86_64 tested only; polling-only async completion; no publish.
No hard RSS/native-OOM containment.

Go handle tokens use atomic ownership. Concurrent Close calls wait for the same completed close operation and return its cached report; Close is idempotent. Call/Create racing with Close return a typed closed or stale-handle error.
