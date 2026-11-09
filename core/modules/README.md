# Offline preparation

`transpile_module` parses JavaScript ESM and transpiles TypeScript/MTS with pinned `deno_ast`. It performs no filesystem or network access. The caller bounds input before parsing; emitted source maps are disabled. JSX/TSX/CommonJS are unsupported.

`Package` verifies each vendored source against a host-provided SHA-256 hex digest and rejects lifecycle scripts and native dependencies. Digests cover content, profile and preparation backend version. The host must obtain expected digests through a trusted channel. Preparation does not establish package provenance. The facade freezes a new graph; existing sandboxes never share module instances.

`PureJsNpm` means host-vendored ESM only. Builtins, `npm:` downloads, native addons, process and network access remain unavailable. This crate does not provide Node compatibility or an npm installer. Module visibility remains enforced by the runtime loader on every resolution.

Run `cargo test --manifest-path core/modules/Cargo.toml` and `cargo clippy --manifest-path core/modules/Cargo.toml --all-targets -- -D warnings`.
