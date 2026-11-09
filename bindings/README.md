# Native binding compatibility

Each binding is a thin adapter to the Rust facade; execution, policy and resource ownership stay in Rust. Python and Node consume the facade directly; Go consumes the explicit C ABI product. Package versions are independent; the initial bindings target facade `API_VERSION` 0.1.0.

| Product | Native integration | Platform / versions | Status |
| --- | --- | --- | --- |
| Rust | direct facade | Linux x86_64, Rust 1.99.1, deno_core 0.413.0 | in-process embedded Deno/V8 (no sidecar); backend fixed per engine (`BackendKind::Deno` default); `Engine::with_host_config` for schemas/audit/shared pools |
| Python | PyO3 / maturin | Linux x86_64, CPython 3.12 | async native wheel built locally; no published wheel; no `HostConfig` controls |
| Node.js | napi-rs | Linux x86_64, Node exactly 24.22.1 | explicit V8 coexistence gate; other versions/platforms fail closed; no `HostConfig` controls |
| C | versioned owned-handle ABI, polling + native host callbacks; `deno_abi_version()` per `bindings/c/include/denosanbox.h` | Linux x86_64, Rust 1.99.1 | `user_data` released after last owner; no deferred tombstones |
| Go | cgo over the C ABI | Linux x86_64, Go 1.27.3 | context-aware polling; native callbacks; context cancellation preserves deadline identity |

CI runs on pushes to `main`, pull requests and manual dispatch, building and testing Rust, Python, Node and the C+Go products on `ubuntu-24.04` with the versions above (see `.github/workflows/ci.yml`). The full guest-language/limit matrix is in [compatibility](../docs/compatibility.md).

Worker, quota and cleanup behavior (managed JSON Workers, live narrowing, per-tree limits, cancellation) is the same facade behavior described there; each binding documents its available ceilings, requests, grants and resource APIs; the advanced `HostConfig` schemas/audit/shared-pool surface only exists in the Rust API.

A C compiler/linker is required for development and cgo; packaged Python/Node native artifacts embed the core. Guests receive no native addons/FFI/process API. Additional OS/architecture/host versions require actual native loading, callback, cancellation and lifecycle tests before advertising support. Python and Node packages are tested from locally built artifacts only; nothing is published by CI. Each binding uses its own Cargo lockfile and pins the core by path/version; they are not forced to share one `deno_core` build, and a binding upgrade does not require a core API change or engine migration.
