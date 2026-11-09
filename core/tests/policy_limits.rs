//! Public-policy runtime quotas: queue admission, message bounds, deadlines,
//! and live policy changes that cannot safely alter allocated resources.

use std::time::Duration;

use denosanbox::policy::Limits;
use denosanbox::{
    BackendKind, Engine, EngineConfig, Error, InvocationOptions, PluginSource, Policy,
    SandboxHandle, SandboxOptions,
};
use serde_json::{json, Value};

const MODULE: &str = r#"
export function echo(value) { return value; }
export function largeOutput() { return "x".repeat(48); }
export function spin() { while (true) {} }
"#;

fn fake_engine() -> Engine {
    Engine::new(EngineConfig::new(BackendKind::Fake)).expect("fake engine")
}

async fn create_with_policy(engine: &Engine, source: &str, policy: Policy) -> SandboxHandle {
    let plugin = engine
        .prepare(PluginSource::single("main.js", source))
        .expect("plugin prepares");
    tokio::time::timeout(
        Duration::from_secs(10),
        engine.create(
            plugin,
            SandboxOptions {
                policy,
                ..SandboxOptions::default()
            },
        ),
    )
    .await
    .expect("sandbox startup is bounded")
    .expect("sandbox creates")
}

/// Wait until the first call has left the queue and is running before adding
/// the queued call. A full-queue response is retried; a pending future means it
/// was admitted behind the deliberately blocked invocation.
async fn enqueue_behind_blocked_call(
    sandbox: &SandboxHandle,
) -> tokio::task::JoinHandle<denosanbox::Result<Value>> {
    for _ in 0..40 {
        let sandbox = sandbox.clone();
        let mut attempt = tokio::spawn(async move {
            sandbox
                .call("echo", json!("queued"), InvocationOptions::default())
                .await
        });
        match tokio::time::timeout(Duration::from_millis(50), &mut attempt).await {
            Err(_) => return attempt,
            Ok(Ok(Err(Error::QueueFull { capacity: 1 }))) => {}
            Ok(outcome) => panic!("queued call completed unexpectedly: {outcome:?}"),
        }
    }
    panic!("running call never made room for one queued invocation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_queue_depth_counts_pending_calls_and_close_accounts_for_cancellations() {
    let engine = fake_engine();
    let policy = Policy::default().with_limits(Limits::default().with_queue_depth(1));
    let sandbox = create_with_policy(&engine, MODULE, policy).await;

    let blocked_sandbox = sandbox.clone();
    let blocked = tokio::spawn(async move {
        blocked_sandbox
            .call("block", Value::Null, InvocationOptions::default())
            .await
    });
    let queued = enqueue_behind_blocked_call(&sandbox).await;

    let third = sandbox
        .call("echo", json!("overflow"), InvocationOptions::default())
        .await
        .expect_err("one running and one queued call must reject a third queued call");
    assert!(
        matches!(&third, Error::QueueFull { capacity: 1 }),
        "queue error must report the effective policy capacity: {third:?}"
    );

    sandbox.cancel();
    let blocked_result = tokio::time::timeout(Duration::from_secs(1), blocked)
        .await
        .expect("blocked call observes cancellation")
        .expect("blocked task joins");
    assert!(
        matches!(&blocked_result, Err(Error::Cancelled)),
        "{blocked_result:?}"
    );
    let queued_result = tokio::time::timeout(Duration::from_secs(1), queued)
        .await
        .expect("queued call is settled during close")
        .expect("queued task joins");
    assert!(
        matches!(&queued_result, Err(Error::Closed)),
        "{queued_result:?}"
    );

    let report = sandbox.close().await.expect("close joins worker");
    assert_eq!(report.invocations_completed, 0);
    assert_eq!(report.invocations_cancelled, 2);
    assert_eq!(engine.active_sandboxes(), 0, "close releases admission");
    assert!(engine.shutdown().await.expect("shutdown").clean);
}

#[cfg(feature = "deno")]
fn assert_message_limit(error: Error) {
    assert!(
        matches!(&error, Error::LimitExceeded { limit, .. } if limit == "max_message_bytes"),
        "expected a typed max_message_bytes error, got {error:?}"
    );
}

#[cfg(feature = "deno")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_message_limit_applies_to_input_output_and_live_narrowing() {
    let engine = Engine::new(EngineConfig::default()).expect("Deno engine");
    let policy = Policy::default().with_limits(Limits::default().with_max_message_bytes(32));
    let sandbox = create_with_policy(&engine, MODULE, policy).await;

    let oversized_input = sandbox
        .call("echo", json!("i".repeat(40)), InvocationOptions::default())
        .await
        .expect_err("input must use the policy message cap");
    assert_message_limit(oversized_input);
    let oversized_output = sandbox
        .call("largeOutput", Value::Null, InvocationOptions::default())
        .await
        .expect_err("serialized output must use the policy message cap");
    assert_message_limit(oversized_output);
    assert_eq!(
        sandbox
            .call("echo", json!("ok"), InvocationOptions::default())
            .await
            .expect("small call fits the initial cap"),
        json!("ok")
    );

    sandbox
        .narrow_policy(&Policy::default().with_limits(Limits::default().with_max_message_bytes(16)))
        .expect("live message cap narrows");
    let narrowed_input = sandbox
        .call("echo", json!("i".repeat(20)), InvocationOptions::default())
        .await
        .expect_err("new input must use the live-narrowed cap");
    assert_message_limit(narrowed_input);
    let narrowed_output = sandbox
        .call("largeOutput", Value::Null, InvocationOptions::default())
        .await
        .expect_err("new output must use the live-narrowed cap");
    assert_message_limit(narrowed_output);
    assert_eq!(
        sandbox
            .call("echo", json!("ok"), InvocationOptions::default())
            .await
            .expect("small call remains allowed after narrowing"),
        json!("ok")
    );

    sandbox.close().await.expect("close");
    assert!(engine.shutdown().await.expect("shutdown").clean);
}

#[cfg(feature = "deno")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_policy_deadline_terminates_javascript_within_bound() {
    let engine = Engine::new(EngineConfig::default()).expect("Deno engine");
    let sandbox = create_with_policy(&engine, MODULE, Policy::default()).await;
    sandbox
        .narrow_policy(
            &Policy::default()
                .with_limits(Limits::default().with_deadline(Duration::from_millis(20))),
        )
        .expect("live deadline narrows");

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        sandbox.call("spin", Value::Null, InvocationOptions::default()),
    )
    .await
    .expect("live deadline terminates the guest within one second");
    assert!(
        matches!(&result, Err(Error::DeadlineExceeded)),
        "expected DeadlineExceeded, got {result:?}"
    );

    sandbox.close().await.expect("close after termination");
    assert!(engine.shutdown().await.expect("shutdown").clean);
}

#[cfg(feature = "deno")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_allocation_limit_shrinks_are_typed_and_leave_policy_unchanged() {
    let engine = Engine::new(EngineConfig::default()).expect("Deno engine");
    let initial = Limits::default();
    let original_policy = Policy::default().with_limits(initial);
    let sandbox = create_with_policy(&engine, MODULE, original_policy.clone()).await;

    let smaller_heap = initial.with_heap_target_bytes(initial.heap_target_bytes() / 2);
    let smaller_tree_memory = initial.with_memory_budget_bytes(initial.memory_budget_bytes() / 2);
    let smaller_sandbox_count = initial.with_max_sandboxes(initial.max_sandboxes() / 2);
    for limits in [smaller_heap, smaller_tree_memory, smaller_sandbox_count] {
        let error = sandbox
            .narrow_policy(&Policy::default().with_limits(limits))
            .expect_err("live allocation/admission bounds cannot be resized safely");
        assert!(
            matches!(&error, Error::UnsupportedFeature { .. }),
            "expected typed UnsupportedFeature, got {error:?}"
        );
        sandbox
            .narrow_policy(&original_policy)
            .expect("rejected shrink must not mutate the current policy")
    }

    assert_eq!(
        sandbox
            .call("echo", json!("ok"), InvocationOptions::default())
            .await
            .expect("sandbox remains usable after rejected policy changes"),
        json!("ok")
    );
    sandbox.close().await.expect("close");
    assert!(engine.shutdown().await.expect("shutdown").clean);
}
