# Native ABI validation — 2026-11-09

Linux x86_64, Rust 1.99.1, Go 1.27.3, production implementation
`41566532a2f00c3eb94f7fe3fcca481f8b66b352`.

```sh
sh bindings/c/test.sh
```

All three standalone C examples compiled with `-std=gnu11 -Wall -Wextra -Werror` and
passed. Edge coverage includes oversize input rejection before pointer access,
NULL/empty slices, wrong-kind/stale tokens, copied buffer/error ownership,
at-most-once job result delivery, early job free, and deterministic close.

The Go adapter passed `go test -race -count=1 ./...`, including terminal context
cancellation, `context.DeadlineExceeded` identity, concurrent/idempotent Close,
and the four common `fixtures/behavior/v1.json` cases through this C ABI.
The standalone Go example also passed. Cargo fmt and clippy `-D warnings`
passed for the native C crate. Execution uses no Deno executable or sidecar.

The host-callback example passed 62 checks, including capability grants, trusted
scope, borrowed-context expiry, input/output ownership, stale handles, and
release after engine free. Registration ownership transfers only on success;
release waits for the final registration/grant/in-flight owner. Non-cooperative
callbacks retain ownership until they actually return, even after cancellation.

Go callback tests cover scoped cancellation and cleanup without persistent
handle tombstones. Deadline/context/cancellation cases also passed ten repeated
race-detector runs. A context's expired deadline maps to
`context.DeadlineExceeded`; an earlier native timeout remains a typed native
error. Polling remains the C ABI's async completion mechanism; callbacks are
synchronous native functions dispatched on managed host threads.

These checks validate the experimental ABI 1 and package 0.1.0 on this platform;
they do not claim native fault/OOM containment or support for other platforms.
