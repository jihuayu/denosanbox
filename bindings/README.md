# Native binding compatibility

Each binding is a thin adapter to the Rust facade. Execution/policy/resource ownership stay in Rust. Python and Node consume the facade directly; Go consumes the explicit C ABI product. Package versions are independent; the current initial bindings target Rust facade 0.1.0.

| Product | Native integration | Built/validated against | Platform | Status |
| --- | --- | --- | --- | --- |
| Rust | direct facade (`API_VERSION` 0.1.0) | Rust 1.99.1, deno_core 0.413.0 | Linux x86_64 | in-process Deno/V8; backend fixed per engine (`BackendKind::Deno` default), no live migration |
| Python | PyO3 / maturin | CPython 3.12, matching core checkout | Linux x86_64 | native async wheel; no published wheel |
| Node.js | napi-rs | Node 24.22.1 | Linux x86_64 | explicit V8 coexistence gate; other versions/platforms fail closed |
| C | versioned owned-handle ABI, `deno_abi_version() == 1` | Rust 1.99.1 | Linux x86_64 | optional polling subset, no callback API |
| Go | cgo over C ABI | Go 1.27.3 | Linux x86_64 | optional; see linking/context docs |

CI builds and tests Rust, Python, Node and the C+Go products on `ubuntu-24.04` with the versions above (see `.github/workflows/ci.yml`). The full guest-language/limit matrix is in [compatibility](../docs/compatibility.md).

A C compiler/linker is required for development and cgo; packaged Python/Node native artifacts embed the core. Guests receive no native addons/FFI/process API. Additional OS/architecture/host versions require actual native loading, callback, cancellation and lifecycle tests before advertising support. No registry release is performed by CI. Each binding uses its own Cargo lockfile and pins the core by path/version; they are not forced to share one `deno_core` build, and a binding upgrade does not require a core API change or engine migration.
