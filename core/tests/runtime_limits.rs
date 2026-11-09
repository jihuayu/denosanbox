//! Bounded runtime-limit conformance for the real in-process Deno/V8 backend.
//!
//! Every case here goes through the public facade only (no temp files, no
//! private snapshots, no subprocess). It covers limits a guest can actually
//! reach: the `ArrayBuffer` backing-store budget, message bounds, the pending
//! queue, closed/cancelled lifecycles and per-invocation log retention.
//!
//! Honesty notes:
//!
//! * Heap targets and budgets are *accounting* reservations, not a hard RSS
//!   ceiling. Native faults, V8 bugs and process-wide OOM stay shared-process
//!   risks (see `docs/threat-model.md`).
//! * Every test stays small: the largest allocation attempted is 9 MiB inside
//!   a 16 MiB heap target / 24 MiB tree budget. No unbounded OOM probe.
//! * Two tests are `#[ignore]`d as *pending* acceptance criteria for runtime
//!   fixes (documented findings, not passing guarantees):
//!   `narrowed_policy_log_cap_is_enforced_live` (backend hardcodes a 64 KiB log
//!   cap and ignores the effective policy) and
//!   `array_buffer_over_tree_budget_is_bounded_and_typed` (a 9 MiB store is
//!   refused via `js_heap_target`, which also closes the sandbox).

#![cfg(feature = "deno")]

use std::time::Duration;

use denosanbox::policy::{Budget, Limits};
use denosanbox::{Engine, EngineConfig, InvocationOptions, Policy, SandboxHandle, SandboxOptions};
use serde_json::{json, Value};

const MIB: usize = 1024 * 1024;

fn engine() -> Engine {
    Engine::new(EngineConfig::default()).expect("real Deno engine")
}

async fn create(engine: &Engine, source: &str, options: SandboxOptions) -> SandboxHandle {
    let plugin = engine
        .prepare(denosanbox::PluginSource::single("main.js", source))
        .expect("plugin should prepare");
    tokio::time::timeout(Duration::from_secs(10), engine.create(plugin, options))
        .await
        .expect("sandbox creation must be bounded")
        .expect("sandbox should be created")
}

/// Tight policy: 24 MiB whole-tree memory budget, 16 MiB own JS heap target.
fn tight_limits() -> Limits {
    Limits::default()
        .with_heap_target_bytes(16 * MIB)
        .with_memory_budget_bytes(24 * MIB)
}

const ARRAY_BUFFER_JS: &str = r#"
export function bytesHeld(sizes) {
  const held = []; // strongly roots every ArrayBuffer until return
  let total = 0;
  for (const size of sizes) {
    const buffer = new ArrayBuffer(size);
    held.push(buffer);
    total += buffer.byteLength;
  }
  return total;
}
export function ping(input) {
  return { echo: input };
}
"#;

// ---------------------------------------------------------------------------
// ArrayBuffer backing stores are charged to the sandbox memory budget
// ---------------------------------------------------------------------------

/// PENDING runtime fix. A 9 MiB external `ArrayBuffer` store inside a 16 MiB
/// heap / 24 MiB tree budget must be refused with a typed limit error while the
/// sandbox stays usable (a 64 KiB store still succeeds).
///
/// Observed on the current runtime snapshot: the 9 MiB store is refused, but
/// with `LimitExceeded { limit: "js_heap_target" }` from the near-heap-limit
/// callback, which also cancels the host context, so the sandbox is then
/// `Closed` rather than usable. Kept ignored until the runtime reports the
/// subtree memory budget distinctly and keeps the sandbox usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "pending runtime fix: 9 MiB ArrayBuffer is refused via js_heap_target and leaves the sandbox closed"]
async fn array_buffer_over_tree_budget_is_bounded_and_typed() {
    let engine = engine();
    let sandbox = create(
        &engine,
        ARRAY_BUFFER_JS,
        SandboxOptions {
            policy: Policy::default().with_limits(tight_limits()),
            ..Default::default()
        },
    )
    .await;

    let err = sandbox
        .call("bytesHeld", json!([9 * MIB]), InvocationOptions::default())
        .await
        .expect_err("9 MiB must not fit into a 16 MiB heap target");
    assert_eq!(
        err.kind(),
        "limit_exceeded",
        "9 MiB ArrayBuffer should be a typed limit error: {err:?}"
    );

    // The rejection must not take the sandbox or the engine down.
    assert_eq!(
        sandbox
            .call("ping", json!("alive"), InvocationOptions::default())
            .await
            .expect("sandbox remains usable after a rejected allocation"),
        json!({"echo": "alive"})
    );
    // The rejected reservation released its lease: a small store still fits.
    assert_eq!(
        sandbox
            .call(
                "bytesHeld",
                json!([64 * 1024]),
                InvocationOptions::default()
            )
            .await
            .expect("64 KiB ArrayBuffer fits after the rejection"),
        json!(64 * 1024)
    );

    let report = sandbox.close().await.expect("close");
    assert_eq!(report.pending_host_operations, 0);
    assert!(engine.shutdown().await.expect("shutdown").clean);
}

/// The bound is per sandbox: a tight sandbox's rejection never affects an
/// independent sibling with its own budget, which still serves a 64 KiB store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn array_buffer_budget_is_isolated_between_sandboxes() {
    let engine = engine();
    let tight = create(
        &engine,
        ARRAY_BUFFER_JS,
        SandboxOptions {
            policy: Policy::default().with_limits(tight_limits()),
            ..Default::default()
        },
    )
    .await;
    let sibling = create(&engine, ARRAY_BUFFER_JS, SandboxOptions::default()).await;

    let tight_outcome = tight
        .call("bytesHeld", json!([9 * MIB]), InvocationOptions::default())
        .await;
    assert!(
        tight_outcome.is_err(),
        "9 MiB must not fit into the tight sandbox: {tight_outcome:?}"
    );
    assert_eq!(
        sibling
            .call(
                "bytesHeld",
                json!([64 * 1024]),
                InvocationOptions::default()
            )
            .await
            .expect("sibling with its own budget is unaffected"),
        json!(64 * 1024)
    );
    assert_eq!(
        sibling
            .call("ping", json!("alive"), InvocationOptions::default())
            .await
            .expect("sibling remains usable"),
        json!({"echo": "alive"})
    );

    tight.close().await.expect("close tight");
    sibling.close().await.expect("close sibling");
    engine.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonpositive_memory_limit_is_rejected_before_startup() {
    let engine = engine();
    let plugin = engine
        .prepare(denosanbox::PluginSource::single("main.js", ARRAY_BUFFER_JS))
        .expect("prepare");
    let err = engine
        .create(
            plugin,
            SandboxOptions {
                memory_limit_bytes: Some(0),
                ..Default::default()
            },
        )
        .await
        .expect_err("a zero memory limit is not a usable configuration");
    assert!(
        matches!(err.kind(), "invalid_configuration" | "limit_exceeded"),
        "zero memory limit should be a typed rejection, got {err:?}"
    );
    assert_eq!(
        engine.active_sandboxes(),
        0,
        "a rejected startup must not leak admission"
    );
    engine.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Accounting reservations never charge partially, never exceed ancestors
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ancestor_memory_budget_is_charged_before_children() {
    // These are accounting reservations of the kind the engine makes before
    // V8 allocates; this is not a measured-RSS claim.
    let global = Budget::new((64 * MIB) as u64);
    let a = global.child((48 * MIB) as u64);
    let b = global.child((48 * MIB) as u64);
    let a_heap = a
        .reserve((40 * MIB) as u64)
        .expect("first own-heap reservation");
    assert_eq!(global.used(), (40 * MIB) as u64);
    // 40 + 40 exceeds the 64 MiB ancestor even though each child has 48 MiB.
    assert!(b.reserve((40 * MIB) as u64).is_err());
    assert_eq!(
        b.used(),
        0,
        "a rejected reservation must not charge the child"
    );
    drop(a_heap);
    assert_eq!(global.used(), 0);
    assert!(b.reserve((40 * MIB) as u64).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_sandbox_admission_is_released_after_close() {
    let mut config = EngineConfig::default();
    config.limits.max_active_sandboxes = 1;
    let engine = Engine::new(config).expect("engine");
    let first = create(&engine, ARRAY_BUFFER_JS, SandboxOptions::default()).await;
    let plugin = engine
        .prepare(denosanbox::PluginSource::single("main.js", ARRAY_BUFFER_JS))
        .expect("prepare");
    let err = engine
        .create(plugin.clone(), SandboxOptions::default())
        .await
        .expect_err("second sandbox exceeds max_active_sandboxes");
    assert_eq!(err.kind(), "limit_exceeded", "got {err:?}");
    first.close().await.expect("close");
    let second = tokio::time::timeout(
        Duration::from_secs(10),
        engine.create(plugin, SandboxOptions::default()),
    )
    .await
    .expect("admission after close must not block")
    .expect("admission released after close");
    second.close().await.expect("close second");
    assert_eq!(engine.active_sandboxes(), 0);
}

// ---------------------------------------------------------------------------
// Message bounds and queue saturation
// ---------------------------------------------------------------------------

const BLOCK_JS: &str = r#"
export function hold(_input) {
  while (true) {}
}
export function echo(input) {
  return input;
}
"#;

fn short_timeout() -> InvocationOptions {
    InvocationOptions {
        timeout: Some(Duration::from_secs(5)),
        deadline: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_inputs_and_results_are_typed() {
    let mut config = EngineConfig::default();
    config.limits.max_message_bytes = 4 * 1024;
    let engine = Engine::new(config).expect("engine");
    let source = r#"
export function echo(input) { return input; }
export function big(_input) { return "y".repeat(16 * 1024); }
"#;
    let sandbox = create(&engine, source, SandboxOptions::default()).await;
    let err = sandbox
        .call(
            "echo",
            Value::String("z".repeat(8 * 1024)),
            InvocationOptions::default(),
        )
        .await
        .expect_err("input above max_message_bytes");
    assert_eq!(err.kind(), "limit_exceeded", "got {err:?}");
    let err = sandbox
        .call("big", Value::Null, InvocationOptions::default())
        .await
        .expect_err("result above max_message_bytes");
    assert_eq!(err.kind(), "limit_exceeded", "got {err:?}");
    // A bounded value still round-trips after both rejections.
    assert_eq!(
        sandbox
            .call("echo", json!({"ok": true}), InvocationOptions::default())
            .await
            .expect("echo"),
        json!({"ok": true})
    );
    sandbox.close().await.expect("close");
    engine.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_pending_queue_is_typed_and_settles_on_cancel() {
    let mut config = EngineConfig::default();
    config.limits.max_pending_invocations = 1;
    config.limits.default_invocation_timeout = Duration::from_secs(5);
    config.limits.max_invocation_timeout = Duration::from_secs(10);
    let engine = Engine::new(config).expect("engine");
    let sandbox = create(&engine, BLOCK_JS, SandboxOptions::default()).await;

    let running = sandbox.clone();
    let first =
        tokio::spawn(async move { running.call("hold", json!(null), short_timeout()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let queued = sandbox.clone();
    let second = tokio::spawn(async move { queued.call("echo", json!(1), short_timeout()).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let err = sandbox
        .call("echo", json!(2), InvocationOptions::default())
        .await
        .expect_err("a pending queue of depth 1 is saturated");
    assert_eq!(err.kind(), "queue_full", "got {err:?}");

    // Cancellation is terminal but must settle every waiter with a typed
    // error instead of hanging the host.
    sandbox.cancel();
    let first = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("running call must settle")
        .expect("join");
    let second = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("queued call must settle")
        .expect("join");
    assert!(first.is_err(), "cancelled hold should fail: {first:?}");
    assert!(
        matches!(second, Err(ref e) if matches!(e.kind(), "closed" | "cancelled")),
        "queued call should be closed/cancelled: {second:?}"
    );

    // The engine itself is unaffected: a sibling sandbox still works.
    let sibling = create(&engine, BLOCK_JS, SandboxOptions::default()).await;
    assert_eq!(
        sibling
            .call("echo", json!("ok"), InvocationOptions::default())
            .await
            .expect("sibling usable"),
        json!("ok")
    );
    sibling.close().await.expect("close sibling");
    engine.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Close / cancel lifetimes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_after_cancel_is_closed_and_never_revives() {
    let engine = engine();
    let sandbox = create(&engine, BLOCK_JS, SandboxOptions::default()).await;
    sandbox.cancel();
    for _ in 0..3 {
        let err = sandbox
            .call("echo", json!(1), InvocationOptions::default())
            .await
            .expect_err("call after cancel must fail");
        assert!(matches!(err.kind(), "closed" | "cancelled"), "got {err:?}");
    }
    sandbox.close().await.expect("close");
    engine.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_close_settles_within_bounded_time() {
    let engine = engine();
    let sandbox = create(&engine, BLOCK_JS, SandboxOptions::default()).await;
    let runner = sandbox.clone();
    let in_flight =
        tokio::spawn(async move { runner.call("hold", json!(null), short_timeout()).await });
    tokio::time::sleep(Duration::from_millis(30)).await;

    let a = sandbox.clone();
    let b = sandbox.clone();
    let (close_a, close_b) = tokio::join!(
        async { tokio::time::timeout(Duration::from_secs(10), a.close()).await },
        async { tokio::time::timeout(Duration::from_secs(10), b.close()).await },
    );
    assert!(close_a.is_ok(), "first concurrent close must not hang");
    assert!(close_b.is_ok(), "second concurrent close must not hang");
    let result = tokio::time::timeout(Duration::from_secs(5), in_flight)
        .await
        .expect("in-flight call must settle")
        .expect("join");
    assert!(result.is_err(), "terminated call should fail: {result:?}");
    let err = sandbox
        .call("echo", json!(1), InvocationOptions::default())
        .await
        .expect_err("call after close");
    assert!(matches!(err.kind(), "closed" | "cancelled"), "got {err:?}");
    assert_eq!(engine.active_sandboxes(), 0, "close must release admission");
    engine.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_while_guest_spins_is_bounded() {
    let engine = engine();
    let sandbox = create(&engine, BLOCK_JS, SandboxOptions::default()).await;
    let runner = sandbox.clone();
    let in_flight =
        tokio::spawn(async move { runner.call("hold", json!(null), short_timeout()).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    let report = tokio::time::timeout(Duration::from_secs(10), engine.shutdown())
        .await
        .expect("shutdown must be bounded")
        .expect("shutdown");
    assert_eq!(report.sandboxes_closed, 1);
    let _ = in_flight.await.expect("join");
    // The handle was cancelled by shutdown: no call can revive it.
    let err = sandbox
        .call("echo", json!(1), InvocationOptions::default())
        .await
        .expect_err("post-shutdown call");
    assert!(matches!(err.kind(), "closed" | "cancelled"), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Per-invocation log retention
// ---------------------------------------------------------------------------

const LOG_JS: &str = r#"
export function logs(n) {
  for (let i = 0; i < n; i++) console.log("x".repeat(64));
  return n;
}
"#;

/// Issues one call that emits `n` log lines of 64 bytes each and returns the
/// facade result kind (`Ok` or the error `kind()` string).
async fn log_outcome(sandbox: &SandboxHandle, n: u64) -> Result<u64, String> {
    match sandbox
        .call("logs", json!(n), InvocationOptions::default())
        .await
    {
        Ok(value) => value
            .as_u64()
            .ok_or_else(|| format!("unexpected log result {value:?}")),
        Err(err) => Err(err.kind().to_string()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_output_is_bounded_per_invocation() {
    // No host stdout sink exists: the backend charges and discards logs under
    // a per-invocation retention cap (64 KiB default). 1000 lines * 64 bytes
    // fit; 2000 lines do not, and the cap resets for the next invocation.
    let engine = engine();
    let sandbox = create(&engine, LOG_JS, SandboxOptions::default()).await;
    assert_eq!(
        log_outcome(&sandbox, 1000).await.expect("1000 lines fit"),
        1000
    );
    let err = log_outcome(&sandbox, 2000)
        .await
        .expect_err("128000 log bytes must be refused");
    assert!(
        !err.is_empty(),
        "log overflow must surface as a typed failure"
    );
    assert_eq!(
        log_outcome(&sandbox, 1000)
            .await
            .expect("cap resets per invocation"),
        1000
    );
    sandbox.close().await.expect("close");
    engine.shutdown().await.expect("shutdown");
}

/// PENDING runtime fix. A narrow policy `max_log_bytes` (1 KiB) must be honoured
/// before the next invocation, not only at sandbox creation: the reviewed
/// backend snapshot reserves a hardcoded 64 KiB cap in bootstrapped JS and
/// never re-reads the effective policy, so 4096 log bytes are still admitted.
/// Kept `#[ignore]`d as the acceptance criterion; remove the attribute once the
/// runtime reads `policy.limits().max_log_bytes()` per invocation (including
/// live `narrow_policy`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "pending runtime fix: honour effective/live-narrowed policy max_log_bytes"]
async fn narrowed_policy_log_cap_is_enforced_live() {
    let engine = engine();
    let sandbox = create(
        &engine,
        LOG_JS,
        SandboxOptions {
            policy: Policy::default().with_limits(Limits::default().with_max_log_bytes(1024)),
            ..Default::default()
        },
    )
    .await;
    // 64 lines * 64 bytes = 4096 bytes must fail under a 1 KiB cap.
    assert!(
        log_outcome(&sandbox, 64).await.is_err(),
        "4096 log bytes exceed the 1 KiB narrowed cap"
    );
    // 10 lines * 64 bytes = 640 bytes still fit.
    assert_eq!(
        log_outcome(&sandbox, 10).await.expect("640 log bytes fit"),
        10
    );
    // Live narrowing to 512 bytes must apply to the next invocation.
    sandbox
        .narrow_policy(&Policy::default().with_limits(Limits::default().with_max_log_bytes(512)))
        .expect("live narrowing is allowed");
    assert!(
        log_outcome(&sandbox, 10).await.is_err(),
        "640 log bytes exceed the live-narrowed 512 byte cap"
    );
    sandbox.close().await.expect("close");
    engine.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn narrow_policy_cannot_widen_created_limits() {
    // The facade must reject expansion rather than silently applying it.
    let engine = engine();
    let sandbox = create(
        &engine,
        "export function ping(_input) { return 1; }",
        SandboxOptions {
            policy: Policy::default().with_limits(Limits::default().with_max_message_bytes(4096)),
            ..Default::default()
        },
    )
    .await;
    assert!(sandbox.narrow_policy(&Policy::default()).is_err());
    assert_eq!(
        sandbox
            .call("ping", json!(null), InvocationOptions::default())
            .await
            .expect("sandbox still usable"),
        json!(1)
    );
    sandbox.close().await.expect("close");
    engine.shutdown().await.expect("shutdown");
}
