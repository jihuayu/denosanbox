//! Conformance for the explicitly bounded host-managed Worker request/reply profile.
#![cfg(feature = "deno")]

use std::{collections::BTreeMap, time::Duration};

use denosanbox::{Engine, EngineConfig, PluginSource, SandboxOptions, WorkerHandle, WorkerOptions};
use serde_json::json;

async fn root(engine: &Engine) -> denosanbox::SandboxHandle {
    let source = PluginSource {
        entrypoint: "root.js".into(),
        modules: BTreeMap::from([
            (
                "root.js".into(),
                "export function ping() { return 'root'; }".into(),
            ),
            (
                "child.js".into(),
                r#"export async function onmessage(value) {
  if (value === 'hold') return new Promise(() => {});
  if (value === 'spin') { while (true) {} }
  if (value === 'large') return 'x'.repeat(1024);
  return { value, kind: 'child' };
}"#
                .into(),
            ),
        ]),
    };
    let plugin = engine.prepare(source).expect("approved module graph");
    tokio::time::timeout(
        Duration::from_secs(3),
        engine.create(plugin, SandboxOptions::default()),
    )
    .await
    .expect("bounded root startup")
    .expect("create root")
}

async fn child(engine: &Engine, parent: &denosanbox::SandboxHandle) -> WorkerHandle {
    WorkerHandle::spawn(
        engine,
        parent,
        "child.js",
        SandboxOptions::default(),
        WorkerOptions::default(),
    )
    .await
    .expect("create supervised child")
}

#[tokio::test(flavor = "current_thread")]
async fn worker_uses_approved_graph_and_parent_close_stops_descendants() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    let parent = root(&engine).await;
    let worker = child(&engine, &parent).await;
    assert_eq!(worker.depth(), parent.depth() + 1);
    assert_eq!(engine.active_sandboxes(), 2);
    assert_eq!(
        worker.post_message(json!({"request": 42})).await.unwrap(),
        json!({"value": {"request": 42}, "kind": "child"})
    );

    let grandchild = child(&engine, worker.sandbox()).await;
    assert_eq!(grandchild.depth(), worker.depth() + 1);
    let close = tokio::time::timeout(Duration::from_secs(3), parent.close())
        .await
        .expect("parent shutdown must be bounded")
        .unwrap();
    assert!(close.closed_cleanly);
    for descendant in [&worker, &grandchild] {
        let error = descendant.post_message(json!(1)).await.unwrap_err();
        assert!(matches!(error.kind(), "closed" | "cancelled"));
        assert!(descendant.close().await.unwrap().closed_cleanly);
    }
    assert_eq!(engine.active_sandboxes(), 0);
    assert!(engine.shutdown().await.unwrap().clean);
}

#[tokio::test(flavor = "current_thread")]
async fn hidden_entrypoint_and_invalid_queues_never_start_an_isolate() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    let parent = root(&engine).await;
    let baseline = engine.active_sandboxes();
    assert!(WorkerHandle::spawn(
        &engine,
        &parent,
        "../hidden.js",
        SandboxOptions::default(),
        WorkerOptions::default(),
    )
    .await
    .is_err());
    assert_eq!(engine.active_sandboxes(), baseline);
    let invalid = WorkerOptions {
        queue_capacity: 0,
        ..WorkerOptions::default()
    };
    let error = WorkerHandle::spawn(
        &engine,
        &parent,
        "child.js",
        SandboxOptions::default(),
        invalid,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), "invalid_configuration");
    assert_eq!(engine.active_sandboxes(), baseline);
    parent.close().await.unwrap();
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_messages_do_not_consume_queue_capacity() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    let parent = root(&engine).await;
    let worker = WorkerHandle::spawn(
        &engine,
        &parent,
        "child.js",
        SandboxOptions::default(),
        WorkerOptions {
            max_message_bytes: 32,
            max_queued_bytes: 64,
            ..WorkerOptions::default()
        },
    )
    .await
    .unwrap();
    let error = worker
        .post_message(json!("x".repeat(40)))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), "limit_exceeded");
    assert_eq!(
        worker.post_message(json!(1)).await.unwrap(),
        json!({"value": 1, "kind": "child"})
    );
    let oversized_reply = worker.post_message(json!("large")).await.unwrap_err();
    assert_eq!(oversized_reply.kind(), "limit_exceeded");
    worker.close().await.unwrap();
    parent.close().await.unwrap();
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn saturated_worker_queue_rejects_without_blocking_shutdown() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    let parent = root(&engine).await;
    let worker = WorkerHandle::spawn(
        &engine,
        &parent,
        "child.js",
        SandboxOptions::default(),
        WorkerOptions {
            queue_capacity: 1,
            invocation_timeout: Duration::from_millis(100),
            ..WorkerOptions::default()
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        let (first, second) = tokio::join!(worker.post_message(json!("spin")), async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            worker.post_message(json!(2)).await
        });
        assert!(
            first.is_err(),
            "unsettled guest callback must not return success"
        );
        assert_eq!(second.unwrap_err().kind(), "queue_full");
        assert!(worker.close().await.unwrap().closed_cleanly);
        parent.close().await.unwrap();
        engine.shutdown().await.unwrap();
    })
    .await
    .expect("queue saturation cannot block the control path");
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_an_inflight_reply_closes_admission_until_native_cleanup() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    let parent = root(&engine).await;
    let worker = child(&engine, &parent).await;
    let dropped = tokio::time::timeout(
        Duration::from_millis(25),
        worker.post_message(json!("spin")),
    )
    .await;
    assert!(
        dropped.is_err(),
        "the infinite guest must not complete normally"
    );
    assert_eq!(
        worker.post_message(json!(1)).await.unwrap_err().kind(),
        "closed"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(3), worker.close())
            .await
            .unwrap()
            .unwrap()
            .closed_cleanly
    );
    assert_eq!(engine.active_sandboxes(), 1);
    parent.close().await.unwrap();
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn queued_byte_budget_counts_inflight_payloads() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    let parent = root(&engine).await;
    let worker = WorkerHandle::spawn(
        &engine,
        &parent,
        "child.js",
        SandboxOptions::default(),
        WorkerOptions {
            queue_capacity: 2,
            max_message_bytes: 8,
            max_queued_bytes: 8,
            invocation_timeout: Duration::from_millis(100),
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        let (first, second) = tokio::join!(worker.post_message(json!("spin")), async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            worker.post_message(json!("a")).await
        });
        assert!(first.is_err());
        assert_eq!(second.unwrap_err().kind(), "limit_exceeded");
        worker.close().await.unwrap();
        parent.close().await.unwrap();
        engine.shutdown().await.unwrap();
    })
    .await
    .expect("byte budget and control path remain bounded");
}
