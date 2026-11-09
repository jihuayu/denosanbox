# Shared behavior contract

`fixtures/behavior/v1.json` is executed through the public facade in `core/tests/parity.rs`, through the Python package in `bindings/python/tests/test_parity.py`, and through the native Node package in `bindings/nodejs/tests/parity.test.cjs`. The testkit crate supplies the backend-neutral Rust fixture schema. Each runner creates two isolates, executes the same steps, compares JSON results/typed error categories and explicitly shuts down. A binding may add platform/event-loop/FFI tests independently. The broader declarative adversarial catalog in `fixtures/conformance` is documentation; runtime assertions live in the Rust integration suites.

## Conformance gates

The reusable Rust conformance gate for the real embedded Deno/V8 backend is the
`core` workspace test set, run with the pinned toolchain:

```sh
cargo test --manifest-path core/Cargo.toml --workspace --all-features --locked
```

- `core/tests/parity.rs` + this crate: the versioned behavior fixture above, run identically by Python and Node.
- `core/tests/runtime_limits.rs`: bounded `ArrayBuffer` memory budget (9 MiB is refused with a typed terminal limit; reuse or retirement accepted, explicit close releases it, siblings/new roots stay usable), message/queue bounds, close/cancel/shutdown lifetimes and per-invocation log retention under the invocation's effective `max_log_bytes`, including live narrowing (no ignored cases).
- `core/tests/host_security.rs`: guest-spoofed identity/scope cannot replace trusted context, ungranted capabilities are denied before dispatch, schema input/result rejection and bounds, metadata-only audit, late-work cancellation and multi-context isolation.
- `core/tests/policy_limits.rs`: queue, message and deadline quotas on queued and direct calls; live narrowing (permissions, messages, queue, log, deadline) and typed rejection of unsupported live heap/tree-memory/sandbox-count changes.
- `core/tests/guest_workers.rs` and `core/tests/workers.rs`: managed JSON request/reply Workers, independent child state, joinable `terminate()`, root-cancel subtree revocation and the `PureJsNpm`-only `node:worker_threads` alias.
- `core/tests/prepared_limits.rs`: graph validation at `create`, including cross-engine engine limits.
- `core/tests/{host_bridge,module_profiles,mounts}.rs` and the `connectors` suite: callback/VFS, language profile, mounts and network.

Limits are accounting bounds, not a hard RSS or native-OOM guarantee; see `docs/threat-model.md`. CI runs the full set on Linux x86_64 with Rust 1.99.1.
