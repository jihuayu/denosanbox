use std::time::Duration;

use denosanbox::{
    BackendKind, Engine, EngineConfig, Error, Feature, InvocationOptions, PluginRequirements,
    PluginSource, SandboxOptions,
};
use serde_json::json;

fn reject_native_code(engine: &Engine) {
    // This source is intentionally invalid. A missing NativeCode capability
    // must be returned before plugin parsing gets a chance to classify it.
    let error = engine
        .prepare_with_requirements(
            PluginSource::single("broken.js", "export function broken( {"),
            &PluginRequirements::new([Feature::NativeCode]),
        )
        .expect_err("native code must be rejected before parsing");

    assert!(matches!(
        error,
        Error::UnsupportedFeature { feature } if feature == Feature::NativeCode.as_str()
    ));
}

#[test]
fn native_code_requirement_is_rejected_before_invalid_source_is_parsed() {
    let fake = Engine::new(EngineConfig::new(BackendKind::Fake)).unwrap();
    reject_native_code(&fake);

    #[cfg(feature = "deno")]
    reject_native_code(&Engine::new(EngineConfig::default()).unwrap());
}

#[test]
fn fake_backend_does_not_advertise_javascript() {
    let engine = Engine::new(EngineConfig::new(BackendKind::Fake)).unwrap();
    let capabilities = engine.capabilities();

    assert_eq!(capabilities.backend, BackendKind::Fake);
    assert!(!capabilities.supports(Feature::JavaScriptEsm));
    assert!(!capabilities.supports(Feature::TypeScript));
    assert!(matches!(
        engine.require_capabilities(&PluginRequirements::new([Feature::JavaScriptEsm])),
        Err(Error::UnsupportedFeature { feature }) if feature == Feature::JavaScriptEsm.as_str()
    ));
}

#[cfg(feature = "deno")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_sum_live_sandboxes_and_close_reports_separate_completed_calls() {
    const SOURCE: &str = r#"
export function allocate(_input) {
  const held = [];
  for (let i = 0; i < 100; i++) held.push(new Array(1000).fill(i));
  return held.length;
}
"#;

    let engine = Engine::new(EngineConfig::default()).unwrap();
    let plugin = engine
        .prepare(PluginSource::single("main.js", SOURCE))
        .unwrap();
    let a = tokio::time::timeout(
        Duration::from_secs(10),
        engine.create(plugin.clone(), SandboxOptions::default()),
    )
    .await
    .expect("first sandbox creation is bounded")
    .unwrap();
    let b = tokio::time::timeout(
        Duration::from_secs(10),
        engine.create(plugin, SandboxOptions::default()),
    )
    .await
    .expect("second sandbox creation is bounded")
    .unwrap();

    for _ in 0..2 {
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            a.call("allocate", json!(null), InvocationOptions::default()),
        )
        .await
        .expect("first sandbox call is bounded")
        .unwrap();
        assert_eq!(result, json!(100));
    }
    for _ in 0..3 {
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            b.call("allocate", json!(null), InvocationOptions::default()),
        )
        .await
        .expect("second sandbox call is bounded")
        .unwrap();
        assert_eq!(result, json!(100));
    }

    let before_close = engine.metrics().await;
    assert_eq!(before_close.completed_invocations, Some(5));
    assert_eq!(before_close.cancelled_invocations, Some(0));

    // Keep `a` alive after close: the aggregate must describe live isolates,
    // not every handle still retained by the caller.
    let a_report = a.close().await.unwrap();
    assert_eq!(a_report.invocations_completed, 2);
    assert_eq!(a_report.invocations_cancelled, 0);
    let after_close = engine.metrics().await;
    assert_eq!(after_close.completed_invocations, Some(3));
    assert_eq!(after_close.cancelled_invocations, Some(0));
    assert_eq!(engine.active_sandboxes(), 1);

    let b_report = b.close().await.unwrap();
    assert_eq!(b_report.invocations_completed, 3);
    assert_eq!(b_report.invocations_cancelled, 0);
    assert_eq!(engine.active_sandboxes(), 0);
}
