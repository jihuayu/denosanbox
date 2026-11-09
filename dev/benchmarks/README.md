# Reproduce native embedding measurements

Run `cargo run --manifest-path core/Cargo.toml --example benchmark -- 1000`. For optimized measurements use `--release` and record that profile separately. Each run executes a native Rust host, prepares an in-memory module, creates an embedded Deno/V8 isolate, validates every returned JSON value, interrupts an infinite loop after a configured 50 ms and explicitly joins shutdown. No Deno executable or sidecar is invoked.

The example emits `results[]` (one measured run with its own profile, platform, toolchain-independent counters and process VmRSS) plus `metadata` describing the example program's own configuration. It never reports hardware capacity.

`linux-x64-debug.json` records implementation `4156653` with the defaults documented in the compatibility matrix. It contains one run on 2026-11-09: Linux x86_64, AMD EPYC 9V74, four CPU quota cores (`cpu.max=400000 100000`), 16 GiB memory cgroup limit, Rust 1.99.1, deno_core 0.413.0, **unoptimized debug build with debuginfo**, 1,000 sequential calls, shared cloud host. There was no explicit warmup; the documentation DSH session ran concurrently, with no native validation builds running. The artifact includes its timestamp and implementation SHA. Measure again on deployment hardware before comparing. Those 4-core/16 GiB figures describe the measurement host, not the capacity or limits of the sandbox. VmRSS is total process resident memory before/while the isolate is live, read from `/proc/self/status`; it includes shared/runtime allocations and does not attribute all memory to the sandbox.

Repeat multiple times on deployment hardware and record CPU, quota, toolchain, backend version, build profile, iteration count, warmup and concurrent workload before comparing results. The default script deliberately leaves release compilation and long stress runs to the operator.

## Host bridge roundtrip benchmark

```sh
cargo run --manifest-path core/Cargo.toml --example interop_benchmark --locked -- 200
# For deployment comparisons, measure an optimized build separately:
cargo run --manifest-path core/Cargo.toml --release --example interop_benchmark --locked -- 200
```

The benchmark uses one engine/isolate, identical JSON payloads of exactly 64,
1,024 and 65,536 bytes, ten warmup calls per mode/size, and 1..=2,000 measured
calls (default 200). It compares pure export echo with an echo through a sync
host callback and an immediately-ready async host callback. It validates every
response and callback count and asserts clean close/shutdown. `wall_elapsed_us`
excludes warmup but includes harness checks; individual invocation latencies
exclude constructing/cloning the caller's input. Percentiles use nearest rank.

`interop-linux-x64-debug.json` retains three actual debug runs and their context.
They measure baseline implementation `58337c3`, before the callback-retirement
and async-cancellation fixes; their implementation and benchmark commits are
recorded separately in the artifact. They are retained as a measured baseline,
not silently relabelled as a measurement of later code.
Each run completed 1,890 invocations including warmups, with 630 sync and 630
async callback dispatches. The table below is the median of the three per-run
p50 values, not a pooled percentile:

| JSON bytes | Export echo | Sync host callback | Ready async host callback |
| --- | --- | --- | --- |
| 64 | 230 µs | 517 µs | 608 µs |
| 1,024 | 361 µs | 614 µs | 758 µs |
| 65,536 | 8,125 µs | 11,589 µs | 12,229 µs |

These full roundtrips include serialization, scheduling, watchdog setup and
guest work. Callback modes additionally include host dispatch; they do not
isolate a single cost. The unoptimized debug profile and concurrent work on a
shared host make these baseline observations unsuitable as release latency or
capacity promises. Use the retained p95/p99 values to see tail variation, and
repeat `--release` on the intended deployment host before setting budgets.

See [host interoperability](../../docs/host-interop.md) for API, executor,
background-task and GC ownership implications.
