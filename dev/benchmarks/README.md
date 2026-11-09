# Reproduce native embedding measurements

Run `cargo run --manifest-path core/Cargo.toml --example benchmark -- 1000`. For optimized measurements use `--release` and record that profile separately. Each run executes a native Rust host, prepares an in-memory module, creates a Deno/V8 isolate, validates every returned JSON value, interrupts an infinite loop after a configured 50 ms and explicitly joins shutdown. No Deno executable is invoked.

`linux-x64-debug.json` is one measured run on 2026-11-09: Linux x86_64, AMD EPYC 9V74, four CPU quota cores (`cpu.max=400000 100000`), 16 GiB memory cgroup limit, Rust 1.99.1, deno_core 0.413.0, debug build, 1,000 sequential calls, shared cloud host. These figures are observations from one run. They are not capacity promises or hard per-isolate RSS bounds. VmRSS is total process resident memory before/while the isolate is live, read from `/proc/self/status`; it includes shared/runtime allocations and excludes a claim that all memory can be attributed to the sandbox.

Repeat multiple times on deployment hardware and record CPU, quota, toolchain, backend version, build profile, iteration count, warmup and concurrent workload before comparing results. The default script deliberately leaves release compilation and long stress runs to the operator.
