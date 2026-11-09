# Boundary fuzz target

Install `cargo-fuzz 0.13.2` and Rust nightly, then run from repository root:

```sh
cargo +nightly fuzz run boundaries --fuzz-dir core/fuzz -- -max_total_time=10 -max_len=8192
```

The instrumented target exercises malformed protocol frames, virtual paths, and permission-intersection invariants. It performs no guest/native I/O. Persist interesting corpus entries separately; a short smoke run is not a security proof. Engine-independent deterministic regression/property tests also run with the normal stable toolchain.
