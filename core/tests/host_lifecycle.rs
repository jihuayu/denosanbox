//! Explicit engine shutdown must release registry-owned host callback captures.
//! These tests observe deterministic drop sentinels owned by the captures;
//! none measures RSS or GC timing, and none leaves a thread blocked.
use denosanbox::*;
#[cfg(feature = "deno")]
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
#[cfg(feature = "deno")]
use std::sync::Mutex;
#[cfg(feature = "deno")]
use std::time::{Duration, Instant};

fn fake_engine() -> Engine {
    Engine::new(EngineConfig::new(BackendKind::Fake)).unwrap()
}

/// A captured value that counts its own release.
struct Tracked(Arc<AtomicUsize>);

impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Waits bounded; panics rather than hang if the condition never holds.
#[cfg(feature = "deno")]
fn wait_until(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_flag(flag: &Mutex<bool>, wake: &Condvar, what: &str) {
    let limit = Instant::now() + Duration::from_secs(5);
    let mut ready = flag.lock().unwrap_or_else(|err| err.into_inner());
    while !*ready {
        let remaining = limit.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for: {what}");
        ready = wake
            .wait_timeout(ready, remaining)
            .unwrap_or_else(|err| err.into_inner())
            .0;
    }
}

#[tokio::test]
async fn explicit_shutdown_releases_tracked_capture() {
    let drops = Arc::new(AtomicUsize::new(0));
    let engine = fake_engine();
    let held = Tracked(drops.clone());
    let registered = engine
        .register_host("held", 64, 64, move |_: &InvocationContext, _: &[u8]| {
            // Force the closure to capture the tracked value.
            let _ = &held;
            Ok(b"null".to_vec())
        })
        .unwrap();
    // The registry owns the closure for the engine's lifetime.
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    let report = engine.shutdown().await.unwrap();
    assert!(report.clean);
    // Engine and registration metadata stay alive; only the capture drops.
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(engine.active_sandboxes(), 0);
    assert_eq!(registered.name(), "held");
    assert_eq!(registered.input_limit(), 64);
    assert_eq!(registered.output_limit(), 64);
    assert!(matches!(
        engine.register_host("after", 64, 64, |_: &InvocationContext, _: &[u8]| {
            Ok(b"null".to_vec())
        }),
        Err(Error::Closed)
    ));
    let _id = registered.id();

    // Repeated and concurrent shutdown stay safe and release nothing again.
    assert!(engine.shutdown().await.unwrap().clean);
    let mut again = Vec::new();
    for _ in 0..2 {
        let engine = engine.clone();
        again.push(tokio::spawn(async move { engine.shutdown().await }));
    }
    for task in again {
        assert!(task.await.unwrap().unwrap().clean);
    }
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_breaks_engine_clone_capture_cycle() {
    let drops = Arc::new(AtomicUsize::new(0));
    let engine = fake_engine();
    let captured = engine.clone();
    let sentinel = Tracked(drops.clone());
    engine
        .register_host("cycled", 64, 64, move |_: &InvocationContext, _: &[u8]| {
            // The engine clone and the sentinel are retained by the registry:
            // EngineInner -> registry -> handler -> EngineInner.
            let _engine = &captured;
            let _sentinel = &sentinel;
            Ok(b"null".to_vec())
        })
        .unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    assert!(engine.shutdown().await.unwrap().clean);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "the registry-held engine clone and sentinel are released"
    );
    assert_eq!(engine.active_sandboxes(), 0);
    assert_eq!(engine.backend(), BackendKind::Fake);
}

#[tokio::test]
async fn capture_destructor_may_reenter_engine_methods() {
    // Detached callbacks are dropped outside the registry lock, so their
    // destructors can take the same lock without self-deadlocking.
    struct Reentrant {
        engine: Engine,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Reentrant {
        fn drop(&mut self) {
            let engine = self.engine.clone();
            assert_eq!(engine.active_sandboxes(), 0);
            assert!(matches!(
                engine.register_host("reentrant", 64, 64, |_: &InvocationContext, _: &[u8]| {
                    Ok(b"null".to_vec())
                }),
                Err(Error::Closed)
            ));
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    let drops = Arc::new(AtomicUsize::new(0));
    let engine = fake_engine();
    let capture = Reentrant {
        engine: engine.clone(),
        drops: drops.clone(),
    };
    engine
        .register_host(
            "reentrant",
            64,
            64,
            move |_: &InvocationContext, _: &[u8]| {
                let _ = &capture;
                Ok(b"null".to_vec())
            },
        )
        .unwrap();
    assert!(engine.shutdown().await.unwrap().clean);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn registration_after_shutdown_is_closed_and_never_runs() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let engine = fake_engine();
    assert!(engine.shutdown().await.unwrap().clean);

    let sync_count = invocations.clone();
    let sync = engine.register_host("sync", 64, 64, move |_: &InvocationContext, _: &[u8]| {
        sync_count.fetch_add(1, Ordering::SeqCst);
        Ok(b"null".to_vec())
    });
    let async_count = invocations.clone();
    let asynchronous = engine.register_host_async("async", 64, 64, move |_, _| {
        async_count.fetch_add(1, Ordering::SeqCst);
        async { Ok(b"null".to_vec()) }
    });
    for result in [sync, asynchronous] {
        match result {
            Err(Error::Closed) => {}
            Err(other) => panic!("expected Error::Closed, got {other:?}"),
            Ok(_) => panic!("registration after shutdown must be rejected"),
        }
    }
    assert_eq!(invocations.load(Ordering::SeqCst), 0);

    // A registration that won the race before the freeze can no longer be
    // granted to a new sandbox after shutdown.
    let late = fake_engine();
    let kept = late
        .register_host("kept", 64, 64, |_: &InvocationContext, _: &[u8]| {
            Ok(b"null".to_vec())
        })
        .unwrap();
    let plugin = late
        .prepare(PluginSource::single(
            "main.js",
            "export async function run(){return await sandbox.host.call('kept',null);}",
        ))
        .unwrap();
    assert!(late.shutdown().await.unwrap().clean);
    let created = late
        .create(
            plugin,
            SandboxOptions {
                policy: Policy::default().allow_capability("kept").unwrap(),
                host_grants: vec![HostGrant {
                    callback: kept,
                    scope: "tenant".into(),
                }],
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(created, Err(Error::Closed)), "{created:?}");
    assert_eq!(invocations.load(Ordering::SeqCst), 0);
}

/// Exercise the actual Engine registry with a live Deno invocation. The gate
/// always ends on timeout or sender drop, including when an assertion panics.
#[cfg(feature = "deno")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_callback_retains_capture_until_real_completion() {
    let policy = Policy::default().allow_capability("slow").unwrap();
    let engine = Engine::with_policy(EngineConfig::default(), policy.clone()).unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let idle_drops = Arc::new(AtomicUsize::new(0));
    let idle = Tracked(idle_drops.clone());
    engine
        .register_host("idle", 64, 64, move |_: &InvocationContext, _: &[u8]| {
            let _ = &idle;
            Ok(b"null".to_vec())
        })
        .unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let tracked = Tracked(drops.clone());
    let callback = engine
        .register_host("slow", 64, 64, move |_: &InvocationContext, _: &[u8]| {
            let _ = &tracked;
            let _ = started_tx.send(());
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3));
            Ok(b"null".to_vec())
        })
        .unwrap();
    let plugin = engine
        .prepare(PluginSource::single(
            "main.js",
            "export async function run(){return await sandbox.host.call('slow',null);}",
        ))
        .unwrap();
    let sandbox = engine
        .create(
            plugin,
            SandboxOptions {
                policy,
                host_grants: vec![HostGrant {
                    callback,
                    scope: "tenant".into(),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let caller = sandbox.clone();
    let invocation = tokio::spawn(async move {
        caller
            .call("run", Value::Null, InvocationOptions::default())
            .await
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let report = tokio::time::timeout(Duration::from_secs(2), engine.shutdown())
        .await
        .unwrap()
        .unwrap();
    let observed = (
        report.clean,
        idle_drops.load(Ordering::SeqCst),
        drops.load(Ordering::SeqCst),
        engine.active_sandboxes(),
    );
    // Release before assertions, even if a captured observation is wrong.
    let _ = release_tx.send(());
    let outcome = tokio::time::timeout(Duration::from_secs(3), invocation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observed, (false, 1, 0, 1));
    assert!(outcome.is_err());
    wait_until(
        "capture and admission release",
        Duration::from_secs(2),
        || drops.load(Ordering::SeqCst) == 1 && engine.active_sandboxes() == 0,
    );
    drop(sandbox);
}
