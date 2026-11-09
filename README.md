# denosanbox

An embeddable Rust library that runs host-prepared JavaScript and TypeScript module graphs in independent Deno/V8 isolates inside the host process. The production backend is embedded `deno_core`; there is no Deno executable, sidecar or subprocess at build or run time. The public API is `Engine → PreparedPlugin → SandboxHandle → Invocation`. Language bindings call the Rust facade directly.

This is an experimental native library. It mediates guest APIs and isolates JavaScript state. Memory limits are finite accounting targets; native faults, aborts and process-wide out-of-memory failures are not contained. See [the threat model](docs/threat-model.md), the [API/lifecycle contract](docs/api.md) and the [compatibility matrix](docs/compatibility.md).

## Platform and toolchain

The only tested and supported platform is **Linux x86_64**, with Rust **1.99.1** (pinned by `rust-toolchain.toml`, including rustfmt and Clippy), `deno_core 0.413.0` (V8 selected by the lockfile), CPython **3.12**, Node **exactly 24.22.1** and Go **1.27.3**. The Node binding loader rejects other platforms and Node versions. No other OS, architecture or tool version is claimed. Python wheels and the npm package are tested from locally built artifacts (local wheel / `npm pack` install); no package is published to any registry by this repository or its CI.

First compilation downloads the pinned V8 native archive and needs a C/C++ toolchain, Python 3 and the platform dependencies required by `deno_core 0.413.0`.

## Run the Rust host

```sh
cargo run --manifest-path core/Cargo.toml --example backend --locked
cargo test --manifest-path core/Cargo.toml --workspace --all-features --locked
cargo clippy --manifest-path core/Cargo.toml --workspace --all-targets --locked -- -D warnings
```

The example prepares an in-memory ESM module, creates two independent sandboxes, observes counters `1, 2` and `1`, closes both and shuts down the engine inside an existing Tokio host. The explicit `fake` backend (`BackendKind::Fake`) exercises lifecycle contracts and does not execute JavaScript.

```rust,ignore
use denosanbox::{Engine, EngineConfig, InvocationOptions, PluginSource, SandboxOptions};
let engine = Engine::new(EngineConfig::default())?;
let plugin = engine.prepare(PluginSource::single(
    "main.ts", "export function run(n: number): number { return n + 1; }",
))?;
let sandbox = engine.create(plugin, SandboxOptions::default()).await?;
let result = sandbox.call("run", serde_json::json!(41), InvocationOptions::default()).await?;
sandbox.close().await?;
engine.shutdown().await?;
```

`Engine::with_host_config(EngineConfig, ceiling: Policy, HostConfig)` is the advanced Rust-only entry point. `HostConfig` carries capability input/output schemas and schema budgets, a shared `HostAudit` sink, the shared host-concurrency pool and a shared managed-memory pool (default 512 MiB, `Budget::new(32)` host slots). The other SDKs do not expose these advanced shared-pool, schema or audit controls; they expose engine ceilings, per-sandbox requests, grants and mounts through their own documented APIs.

## Bindings

- [Python](bindings/python/README.md): PyO3/maturin, asyncio, explicit close/shutdown.
- [Node.js](bindings/nodejs/README.md): napi-rs, Promises, explicit native loading and a strict platform/Node gate.
- [C](bindings/c/README.md): versioned owned-handle ABI (`deno_abi_version()`, see `include/denosanbox.h` for the current value), polling completion and native host callbacks.
- [Go](bindings/go/README.md): cgo over the C ABI, context-aware calls and native callbacks.

The Rust facade and each language package are versioned independently. [Binding compatibility](bindings/README.md) records the tested combinations. The worker limits, quotas and cleanup behavior are in [the compatibility matrix](docs/compatibility.md).

## Modules and guest APIs

Modules are supplied as immutable, host-prepared graphs. Module graph validation happens at `create` (and re-validates a `PreparedPlugin` against the creating engine's limits, including cross-engine limits); static imports, re-exports and computed dynamic imports resolve only inside the frozen graph and ordinary relative paths — nothing is downloaded. JS/MJS and TS/MTS (transpiled, not type-checked) are supported; JSX/TSX, CommonJS, JSON modules, WASM and native addons are rejected. Only approved builtins (the explicit `worker_threads` alias in the `PureJsNpm` profile) are available. The [offline package preparation](core/modules/README.md) verifies SHA-256 digests and rejects lifecycle scripts/native dependencies; there is no npm installer and no runtime package download. [Real npm fixtures](fixtures/npm/sources.json) exercise dequal and nanoid/non-secure.

Guest APIs are explicit capabilities (`sandbox.{fs,env,host,cwd,chdir,exit}`, policy-gated `fetch`, managed Workers). The runtime never exposes raw `Deno.core`, host process environment, signals, subprocess creation, FFI, inspector or shared memory (`WebAssembly`/`SharedArrayBuffer`/`Atomics` are removed). Guest message/queue, permission, log and deadline self-narrowing applies to subsequent checks; live heap, tree-memory and sandbox-count changes are explicitly unsupported (recreate the sandbox). Existing children retain their creation ceilings and a root cancel revokes its whole subtree.

Managed guest Workers are a bounded JSON request/reply subset. During the child's message handler the guest may `postMessage` JSON and receive replies; there are no unsolicited top-level pushes, timers, streaming, `eval`/classic Workers, transferables, `SharedArrayBuffer`, `sharedEnv` or native Workers. The browser `Worker` `terminate()` returns a joinable Promise. The `node:worker_threads` alias exists only in the explicit `PureJsNpm` profile.

Network access is deny-by-default: host, every DNS answer, redirect, port, private-address grant and byte/deadline bound must be explicit. A configured proxy the connector cannot support fails closed instead of connecting directly. Each DNS query keeps its own native-completion admission until native cleanup; unrelated queries do not inherit it. Mounts use no-follow capability handles with per-owner generation checks.

## Limits

Defaults are set so the default sandbox count is usable: the global active sandbox count is 4 and includes descendants; each isolate has a 64 MiB JS heap target with finite 4 MiB termination headroom; the per-tree managed-memory budget is 256 MiB; the shared Rust `HostConfig` managed-memory pool is 512 MiB. Explicitly smaller shared pools are never auto-expanded. These are accounting targets, not hard RSS limits, and do not contain native OOM or crashes. Per-sandbox policy limits (`max_sandboxes`, `queue_depth`, deadline, message, log, heap target, memory budget) are enforced on queued and direct calls and can only narrow along a tree.

## Validation

[Shared behavior fixtures](core/testkit/README.md) run through the Rust, Python and Node native APIs. Rust integration suites cover state/module isolation, policy denial and quotas, cancellation, deadlines, resource ownership, managed Workers, guest-worker limits, host security and teardown. [Boundary fuzzing](core/fuzz/README.md) exercises malformed frames, paths and permission intersections. [Benchmarks](dev/benchmarks/README.md) record commands, hardware/configuration and one measured run (an observation, not a capacity claim); current numbers should be measured anew.

Final validation at implementation commit `4156653`: clean `cargo fmt --check` and all-target Clippy `-D warnings` pass; Rust `--all-features` 211 passed and `--no-default-features` 155 passed (one ignored upstream doc-test in each); `dev/validate-fixtures.py` 8 passed; Node 22/22 tests with no skips plus an installed-`npm pack` smoke test; C/Go `bindings/c/test.sh` and the Go race deadline/cancel tests 10× pass. The Python final rerun is pending a fixture correction and the fuzz job is pending CI; see [Python validation](bindings/python/VALIDATION.md).

Reproduce the final validation commands:

```sh
cargo test --manifest-path core/Cargo.toml --workspace --all-features --locked
cargo test --manifest-path core/Cargo.toml --no-default-features -p denosanbox -p denosanbox-core -p denosanbox-policy -p denosanbox-modules --locked
python3 dev/validate-fixtures.py
(cd bindings/python && maturin build --locked --out wheels && python -m venv .venv && .venv/bin/pip install wheels/*.whl pytest pytest-asyncio && .venv/bin/python -m pytest tests)
(cd bindings/nodejs && npm run build && npm test)
sh bindings/c/test.sh
```

CI (`.github/workflows/ci.yml`) runs on push to `main`, pull requests and manual dispatch: Rust 1.99.1 (`fmt`, `clippy`, all-features and no-default-features tests, example, benchmark smoke, fixture validator) plus Node 24.22.1 and CPython 3.12 on `ubuntu-24.04`, the C ABI + Go adapter via `bindings/c/test.sh` (Go 1.27.3), and a nightly fuzz smoke target. The larger `fixtures/conformance` catalog is validated structurally by `dev/validate-fixtures.py`, which never executes JavaScript. No registry publishing or deployment happens from this repository.

Known limits (no hard RSS or native-OOM containment; no CPU isolation or timeout rollback; no exec/timers/streaming in Workers) are listed in [the threat model](docs/threat-model.md#known-gaps).
