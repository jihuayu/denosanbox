# Native binding compatibility

Each binding is a thin adapter to the Rust facade. Execution/policy/resource ownership stay in Rust. Python and Node consume the facade directly; Go consumes the explicit C ABI product. Package versions are independent; the current initial bindings target Rust facade 0.1.0.

| Product | Native integration | Validated host | Status |
| --- | --- | --- | --- |
| Rust | direct facade | Rust 1.99.1, Linux x86_64 | in-process Deno/V8 |
| Python | PyO3 / maturin | CPython 3.12, Linux x86_64 | native async wheel; see package tests |
| Node.js | napi-rs | Node 24.22.1, Linux x86_64 | explicit V8 coexistence gate; other versions fail closed |
| C | versioned owned-handle ABI | Linux x86_64 | optional; see binding-specific ownership docs |
| Go | cgo over C ABI | Go 1.27.3, Linux x86_64 | optional; see linking/context docs |

A C compiler/linker is required for development and cgo; packaged Python/Node native artifacts embed the core. Guests receive no native addons/FFI/process API. Additional OS/architecture/host versions require actual native loading, callback, cancellation and lifecycle tests before advertising support. No registry release is performed by CI.
