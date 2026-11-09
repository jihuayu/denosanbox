# Native ABI validation — 2026-11-09

Linux x86_64, Rust 1.99.1, Go 1.27.3, production implementation
`b11bb6c2a45f13bc061b64f8e5ffdb3787bfa47b`.

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
exactly-once release. Registration ownership transfers only on success.
`deno_engine_close` retires callback registrations, so release may run during
close before `deno_engine_free`; an in-flight callback retains its owner until
it actually returns, even after cancellation. Free still releases the engine
token and does not trigger a second callback release.

Go callback tests cover scoped cancellation and cleanup without persistent
handle tombstones. Deadline/context/cancellation cases also passed ten repeated
race-detector runs. A context's expired deadline maps to
`context.DeadlineExceeded`; an earlier native timeout remains a typed native
error. Polling remains the C ABI's async completion mechanism; callbacks are
synchronous native functions dispatched on managed host threads.

These checks validate the experimental ABI 1 and package 0.1.0 on this platform;
they do not claim native fault/OOM containment or support for other platforms.
