# denosanbox thin Go adapter (experimental)

This package is cgo-only glue over the C ABI in `bindings/c`. It contains no
JavaScript runtime, sandbox policy, job scheduler or subprocess logic. Calls and
cancellation are delegated to the native core; async completion currently uses
`deno_job_wait` polling because the C callback API is intentionally not exposed
until core callback parity and the release gate are complete.

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
error. A cancelled sandbox cannot be reused; later calls report `StatusClosed`
or `StatusCancelled`.

`Close` returns the core close/shutdown report. A report with `ClosedCleanly` or
`Clean` false is still returned as data (not silently rewritten); the typed error
is reserved for actual close/free failures.

## Test and example

```sh
cd bindings/go
go test ./...

cd ../../examples/go
go run .
```

`go run` here only launches the example program; the adapter itself never shells
out. Both sandboxes live in the example's host PID, as required by the native
core architecture. No guest PID is claimed.

Limitations: Linux x86_64 tested only; polling-only async subset; no guest
callbacks; no publish.

Go handle tokens use atomic ownership. Concurrent Close calls wait for the same completed close operation and return its cached report; Close is idempotent. Call/Create racing with Close return a typed closed or stale-handle error.
