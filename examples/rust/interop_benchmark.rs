//! Reproducible host/sandbox interop latency benchmark on the real backend.
//!
//! Measures full roundtrip latency of sequential, successful calls only (no
//! deadline probes, no concurrency): host->guest pure echo, guest->sync host
//! callback, and guest->async host callback that is immediately ready. Every
//! mode receives the identical payload object; both callbacks return their
//! validated input bytes unchanged under explicit per-sandbox grants.
//!
//! Latencies include JSON serialization, scheduling, guest execution and
//! callbacks: they are not isolated FFI cost and say nothing about capacity.
//! Prints one JSON document on stdout. Run from `core/` with:
//!   cargo run --release --example interop_benchmark [-- ITERATIONS]

use denosanbox::{
    Engine, EngineConfig, HostGrant, InvocationContext, InvocationOptions, PluginSource, Policy,
    SandboxHandle, SandboxOptions,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

const SYNC: &str = "bench.sync";
const ASYNC: &str = "bench.async";
const WARMUP: u32 = 10;
const MAX_ITERS: u32 = 2000;
/// Exact serialized sizes; all fit the default 1 MiB message cap.
const SIZES: [(&str, usize); 3] = [("small_64B", 64), ("1KiB", 1024), ("64KiB", 65_536)];

const PLUGIN: &str = r#"
export function echo(input) { return input; }
export async function callSync(input) { return await sandbox.host.call('bench.sync', input); }
export async function callAsync(input) { return await sandbox.host.call('bench.async', input); }
"#;

/// `{"data":"..."}` whose serialization is exactly `total` bytes.
fn payload(total: usize) -> Value {
    let envelope = serde_json::to_vec(&json!({ "data": "" }))
        .expect("envelope")
        .len();
    let value = json!({ "data": "x".repeat(total - envelope) });
    assert_eq!(serde_json::to_vec(&value).expect("payload").len(), total);
    value
}

#[derive(Default)]
struct Stats {
    calls: u64,
    dispatches: u64,
    wire_bytes: usize,
    /// Measured-call latencies in microseconds (the clock, never wall totals).
    latencies_us: Vec<u64>,
}

impl Stats {
    fn finish(mut self, expected_per_call: u64) -> Value {
        assert_eq!(self.dispatches, expected_per_call * self.calls, "dispatch");
        // Nearest rank: index ceil(n * p / 100) - 1 of the sorted sample, at
        // least 0. Defined for n >= 1, which holds for iterations >= 1.
        self.latencies_us.sort_unstable();
        let n = self.latencies_us.len();
        let pct = |p: usize| self.latencies_us[(n * p).div_ceil(100).max(1) - 1];
        json!({
            "completed_calls": self.calls,
            "host_callback_dispatches": self.dispatches,
            "expected_dispatches": expected_per_call * self.calls,
            "serialized_input_bytes": self.wire_bytes / self.calls as usize,
            "latency_us": { "p50": pct(50), "p95": pct(95), "p99": pct(99) }
        })
    }
}

/// Runs `WARMUP + iterations` calls of `export` with one shared `input`.
/// Only calls dispatched `expected` times are measured.
async fn run(
    sandbox: &SandboxHandle,
    export: &str,
    input: &Value,
    expected: u64,
    counters: (&Arc<AtomicU64>, &Arc<AtomicU64>),
    iterations: u32,
) -> Result<Stats, Box<dyn std::error::Error>> {
    let dispatched = || counters.0.load(Ordering::SeqCst) + counters.1.load(Ordering::SeqCst);
    let (mut stats, mut previous) = (Stats::default(), dispatched());
    for i in 0..WARMUP + iterations {
        let start = Instant::now();
        let result = sandbox
            .call(export, input.clone(), InvocationOptions::default())
            .await?;
        let elapsed_us = start.elapsed().as_micros() as u64;
        let now = dispatched();
        assert_eq!(now - previous, expected, "{export} dispatch");
        previous = now;
        assert_eq!(&result, input, "{export} returns its input");
        if i < WARMUP {
            continue;
        }
        stats.wire_bytes += serde_json::to_vec(&result)?.len();
        stats.latencies_us.push(elapsed_us);
        stats.calls += 1;
        stats.dispatches += expected;
    }
    Ok(stats)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let iterations: u32 = match std::env::args().nth(1) {
        None => 200,
        Some(arg) => arg.parse()?,
    };
    if iterations == 0 || iterations > MAX_ITERS {
        return Err(format!("iterations must be 1..={MAX_ITERS}").into());
    }
    let ceiling = Policy::default()
        .allow_capability(SYNC)?
        .allow_capability(ASYNC)?;
    let engine = Engine::with_policy(EngineConfig::default(), ceiling.clone())?;
    let (sync_calls, async_calls) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let limit = SIZES.iter().map(|(_, size)| *size).max().unwrap_or(0) + 256;
    let sync_counter = Arc::clone(&sync_calls);
    // Both callbacks only return their already-validated input bytes unchanged.
    let sync = engine.register_host(SYNC, limit, limit, move |ctx, input: &[u8]| {
        ctx.check_live()?;
        sync_counter.fetch_add(1, Ordering::SeqCst);
        Ok(input.to_vec())
    })?;
    let async_counter = Arc::clone(&async_calls);
    let async_ = engine.register_host_async(ASYNC, limit, limit, move |ctx, input: Vec<u8>| {
        let counter = Arc::clone(&async_counter);
        async move {
            ctx.check_live()?;
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(input)
        }
    })?;
    let plugin = engine.prepare(PluginSource::single("main.js", PLUGIN))?;
    let sandbox = engine
        .create(
            plugin,
            SandboxOptions {
                policy: ceiling,
                host_grants: vec![
                    HostGrant {
                        callback: sync,
                        scope: "bench".into(),
                    },
                    HostGrant {
                        callback: async_,
                        scope: "bench".into(),
                    },
                ],
                ..Default::default()
            },
        )
        .await?;
    let mut rows = Vec::new();
    for (size, total) in SIZES {
        let input = payload(total);
        for (export, mode, expected) in [
            ("echo", "host_to_guest_echo", 0),
            ("callSync", "guest_to_sync_host_callback", 1),
            ("callAsync", "guest_to_async_host_callback_ready", 1),
        ] {
            let counters = (&sync_calls, &async_calls);
            let stats = run(&sandbox, export, &input, expected, counters, iterations).await?;
            let mut row = stats.finish(expected);
            row["size"] = json!(size);
            row["mode"] = json!(mode);
            row["iterations"] = json!(iterations);
            rows.push(row);
        }
    }
    let close = sandbox.close().await?;
    let shutdown = engine.shutdown().await?;
    let (sync_total, async_total) = (
        sync_calls.load(Ordering::SeqCst),
        async_calls.load(Ordering::SeqCst),
    );
    assert!(
        close.closed_cleanly && !close.forced_termination,
        "clean close"
    );
    assert_eq!(
        close.pending_host_operations, 0,
        "no pending host operations"
    );
    assert!(shutdown.clean, "clean shutdown");
    println!(
        "{}",
        serde_json::to_string(&json!({
            "metadata": {
                "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
                "platform": std::env::consts::OS, "arch": std::env::consts::ARCH,
                "backend": "deno", "iterations": iterations, "warmup_per_mode_size": WARMUP,
                "engines": 1, "sandboxes": 1, "host_callbacks": 2,
                "guest_exports": ["echo", "callSync", "callAsync"],
                "message_cap": "default, not overridden (64KiB fits)",
                "payload": "identical {\"data\":<x bytes>} per size, shared by all modes",
            },
            "results": rows,
            "teardown": {
                "sync_host_callback_calls_total": sync_total,
                "async_host_callback_calls_total": async_total,
                "close": { "closed_cleanly": close.closed_cleanly, "invocations_completed": close.invocations_completed,
                           "forced_termination": close.forced_termination, "pending_host_operations": close.pending_host_operations },
                "shutdown": { "clean": shutdown.clean, "sandboxes_closed": shutdown.sandboxes_closed },
            },
            "explanation": [
                "Latencies are full roundtrip: host JSON serialization, sandbox dispatch, guest execution \
                 (including sandbox.host.call for callback modes), host callback dispatch and response \
                 serialization/deserialization. They are not isolated FFI cost.",
                "All modes use the same guest signatures and the same payload value; callbacks return their \
                 validated input bytes unchanged under explicit grants.",
                "Sequential successful calls only, after 10 warmup calls per mode and size; no deadline probes \
                 or concurrency. Percentiles use nearest rank: ceil(n*p/100)-1 in a sorted sample.",
                "latency_us percentiles cover measured calls only; teardown callback totals include warmups.",
                "No capacity, throughput ceiling or hardware claims are made."
            ]
        }))?
    );
    Ok(())
}
