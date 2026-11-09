//! Explicit engine shutdown must release registry-owned host callback captures.
//! These tests observe deterministic drop sentinels owned by the captures;
//! none measures RSS or GC timing, and none leaves a thread blocked.
use denosanbox::*;
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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

/// A synchronous callback still running when the engine shuts down keeps its
/// capture until it really returns; shutdown must complete without waiting
/// for it, and must still release the other drained handler captures it owns.
/// `HostContext` is bound to the registry it is given, so the engine exercised
/// here is bound to the registry that drives its callback; shutdown's
/// release-before-wait behaviour does not depend on registry ownership.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_callback_retains_capture_until_real_completion() {
    let started = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let idle_drops = Arc::new(AtomicUsize::new(0));

    let policy = Policy::default().allow_capability("slow").unwrap();
    let hosts = Arc::new(Mutex::new(HostRegistry::default()));
    let engine = fake_engine();
    // An idle registration the engine must release on shutdown even while the
    // gated callback is still running.
    let idle = Tracked(idle_drops.clone());
    engine
        .register_host("idle", 64, 64, move |_: &InvocationContext, _: &[u8]| {
            let _ = &idle;
            Ok(b"null".to_vec())
        })
        .unwrap();
    let started_cb = started.clone();
    let release_cb = release.clone();
    let tracked = Tracked(drops.clone());
    let callback = hosts
        .lock()
        .unwrap()
        .register("slow", 64, 64, move |_: &InvocationContext, _: &[u8]| {
            let _captured = &tracked;
            let (lock, wake) = &*started_cb;
            *lock.lock().unwrap_or_else(|err| err.into_inner()) = true;
            wake.notify_all();
            // Non-cooperative until the test releases it.
            let (lock, wake) = &*release_cb;
            let mut released = lock.lock().unwrap_or_else(|err| err.into_inner());
            while !*released {
                released = wake.wait(released).unwrap_or_else(|err| err.into_inner());
            }
            Ok(b"null".to_vec())
        })
        .unwrap();
    let context = denosanbox_core::HostContext::new(
        policy,
        hosts.clone(),
        vec![HostGrant {
            callback,
            scope: "tenant".into(),
        }],
    )
    .unwrap();
    let invocation = {
        let context = context.clone();
        tokio::spawn(async move { context.call("slow".to_string(), Value::Null).await })
    };
    wait_flag(&started.0, &started.1, "callback start");

    // Shutdown completes without waiting for the running callback and drops
    // the drained captures it owns; a repeated shutdown is also safe.
    tokio::time::timeout(Duration::from_secs(5), engine.shutdown())
        .await
        .expect("shutdown must not wait for non-cooperative host work")
        .unwrap();
    assert_eq!(idle_drops.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(5), engine.shutdown())
        .await
        .expect("repeated shutdown must not wait either")
        .unwrap();

    // The in-flight dispatch owns its own handler for the whole run.
    let retired = hosts.lock().unwrap().retire_callbacks();
    assert_eq!(retired.len(), 1);
    drop(retired);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        0,
        "a running dispatch keeps the callback capture until it returns"
    );

    // Bounded cleanup: release the callback, then observe real completion.
    *release.0.lock().unwrap_or_else(|err| err.into_inner()) = true;
    release.1.notify_all();
    let outcome = tokio::time::timeout(Duration::from_secs(5), invocation)
        .await
        .expect("invocation must settle once the callback returns")
        .unwrap();
    assert!(outcome.is_err(), "a closed sandbox must not report success");
    wait_until("capture release", Duration::from_secs(5), || {
        drops.load(Ordering::SeqCst) == 1
    });
    wait_until(
        "sandbox bookkeeping released",
        Duration::from_secs(5),
        || hosts.lock().unwrap().retired_owners() == 0,
    );
}
