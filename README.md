# denosanbox

An embeddable Rust library that runs JavaScript and TypeScript modules in independent Deno/V8 isolates inside the host process. The public API is `Engine → PreparedPlugin → SandboxHandle → Invocation`. Language bindings use the Rust facade directly; execution does not spawn a Deno executable or sidecar.

This is an experimental native library. It mediates guest APIs and isolates JavaScript state. Native faults, aborts and process-wide out-of-memory failures can still affect the host. See [the threat model](docs/threat-model.md) and [API/lifecycle contract](docs/api.md).

## Run the Rust host

Install Rust 1.99.1, a C/C++ toolchain, Python 3 and the platform dependencies required by `deno_core 0.413.0`. The repository toolchain file installs rustfmt and Clippy. First compilation downloads the pinned V8 native archive. Linux x86_64 is the validated native platform.

```sh
cargo run --manifest-path core/Cargo.toml --example backend --locked
cargo test --manifest-path core/Cargo.toml --workspace --locked
cargo clippy --manifest-path core/Cargo.toml --workspace --all-targets --locked -- -D warnings
```

The example prepares an in-memory ESM module, creates two independent sandboxes, observes counters `1, 2` and `1`, closes both and shuts down the engine. It works inside an existing Tokio host. The explicit `fake` backend exercises lifecycle contracts and does not execute JavaScript.

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

## Native bindings

- [Python](bindings/python/README.md): PyO3/maturin, async calls, explicit close/shutdown.
- [Node.js](bindings/nodejs/README.md): napi-rs, Promises, explicit native loading/platform gate.
- C and Go are optional ABI products under `bindings/`; their own documentation defines supported versions, ownership and linking requirements.

The Rust facade and each language package have independent versioning. [Binding compatibility](bindings/README.md) records the tested combinations. Nothing is automatically published to a package registry by this repository.

## Modules and guest APIs

Modules are supplied in immutable, host-prepared graphs. Static imports, re-exports and computed dynamic imports cannot load outside the graph or download files. JS/MJS and TS/MTS are supported; TypeScript is transpiled, not type-checked. JSX, CommonJS and native addons are unsupported.

[Offline package preparation](core/modules/README.md) verifies SHA-256 digests and rejects lifecycle scripts/native dependencies. The audited npm profile supports vendored ESM; [real npm fixtures](fixtures/npm/sources.json) exercise dequal and nanoid/non-secure. This does not enable a package manager or unrestricted Node compatibility.

Guest APIs are explicit capabilities. The runtime never exposes raw `Deno.core`, host environment variables, process signals, subprocess creation, FFI, inspector or shared memory. See [the compatibility matrix](docs/compatibility.md) for the supported surface and failure behavior.

## Validation

[Shared behavior fixtures](core/testkit/README.md) run through Rust, Python and Node native APIs. Rust integration suites cover state/module isolation, policy denial, cancellation, deadlines, resource ownership and teardown. [Boundary fuzzing](core/fuzz/README.md) exercises malformed frames, paths and permission intersections. [Benchmarks](dev/benchmarks/README.md) include commands, hardware/configuration and measured JSON output.

Engine-independent checks do not require V8:

```sh
cargo test --manifest-path core/Cargo.toml --no-default-features \
  -p denosanbox -p denosanbox-core -p denosanbox-policy -p denosanbox-modules --locked
python3 dev/validate-fixtures.py
```

The larger `fixtures/conformance` catalog contains additional declarative scenarios; its Python validator checks structure and never claims JavaScript execution.
