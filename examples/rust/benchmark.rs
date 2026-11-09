//! Reproducible single-process measurements; no capacity or RSS guarantees.
//!
//! Output convention: `results[]` holds only measured runs with their own
//! environment (profile, hardware, host configuration, measured values).
//! `metadata` describes this example program itself (its original run was a
//! debug build; tests and benchmarks are compiled without debug info, see the
//! workspace CI env) so a reader never mistakes it for the capacity of the
//! hardware it happened to run on.
use denosanbox::{Engine, EngineConfig, InvocationOptions, PluginSource, SandboxOptions};
use serde_json::json;
use std::time::{Duration, Instant};

fn rss_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")
                .and_then(|value| value.split_whitespace().next()?.parse().ok())
        })
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let iterations: u32 = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1000".into())
        .parse()?;
    if iterations == 0 || iterations > 100_000 {
        return Err("iterations must be 1..=100000".into());
    }
    let engine = Engine::new(EngineConfig::default())?;
    let rss_before = rss_kib();
    let prepare_start = Instant::now();
    let plugin = engine.prepare(PluginSource::single("main.js", "export function run(input) { return input + 1; } export function spin() { while (true) {} }"))?;
    let prepare_us = prepare_start.elapsed().as_micros();
    let start = Instant::now();
    let sandbox = engine.create(plugin, SandboxOptions::default()).await?;
    let startup_us = start.elapsed().as_micros();
    let start = Instant::now();
    for n in 0..iterations {
        let result = sandbox
            .call("run", json!(n), InvocationOptions::default())
            .await?;
        assert_eq!(result, json!(n + 1));
    }
    let elapsed = start.elapsed();
    let rss_live = rss_kib();
    let start = Instant::now();
    let error = sandbox
        .call(
            "spin",
            json!(null),
            InvocationOptions {
                timeout: Some(Duration::from_millis(50)),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), "deadline_exceeded");
    let deadline_observed_us = start.elapsed().as_micros();
    let report = sandbox.close().await?;
    let shutdown = engine.shutdown().await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "results": [{
                "profile": if cfg!(debug_assertions) {"debug"} else {"release"},
                "platform": std::env::consts::OS, "arch": std::env::consts::ARCH,
                "iterations": iterations, "prepare_us": prepare_us, "startup_us": startup_us,
                "calls_elapsed_us": elapsed.as_micros(), "calls_per_second": f64::from(iterations)/elapsed.as_secs_f64(),
                "deadline_configured_us": 50000, "deadline_observed_us": deadline_observed_us,
                "process_rss_before_kib": rss_before, "process_rss_live_kib": rss_live,
                "close_clean": report.closed_cleanly, "shutdown_clean": shutdown.clean
            }],
            // Describes this example program, not the hardware or a capacity
            // promise. The first published run (dev/benchmarks) used this
            // configuration on a shared 4-core/16 GiB cloud host.
            "metadata": {
                "code_configuration": "single in-memory module; default EngineConfig (64 MiB heap target, 1 MiB messages, 30 s default timeout); fixed 50 ms infinite-loop deadline probe; process VmRSS before/with one live isolate",
                "original_run_note": "debug build, 1000 sequential calls, 2026-11-09; observations, not a capacity claim",
                "release_used_for_optimized_runs": false,
                "note": "run with --release and record CPU/quota/build profile for optimized numbers"
            }
        }))?
    );
    Ok(())
}
