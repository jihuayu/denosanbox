# Native ABI validation — 2026-11-09

Linux x86_64, Rust 1.99.1, Go 1.27.3, core commit d4855fe.

```sh
sh bindings/c/test.sh
```

Both standalone C examples compiled with `-std=c11 -Wall -Wextra -Werror` and
passed. Edge coverage includes oversize input rejection before pointer access,
NULL/empty slices, wrong-kind/stale tokens, copied buffer/error ownership,
at-most-once job result delivery, early job free, and deterministic close.

The Go adapter passed `go test -race -count=1 ./...`, including terminal context
cancellation, `context.DeadlineExceeded` identity, concurrent/idempotent Close,
and the four common `fixtures/behavior/v1.json` cases through this C ABI.
The standalone Go example also passed. Cargo fmt and clippy `-D warnings`
passed for the native C crate. Execution uses no Deno executable or sidecar.

C callbacks are not exposed in this initial polling subset. No C/Go callback
lifetime or callback parity result is claimed by these checks.
