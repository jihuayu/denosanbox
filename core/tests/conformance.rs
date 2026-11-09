//! End-to-end conformance tests for the `denosanbox` facade.
//!
//! These exercise the public API surface against whatever backend
//! `EngineConfig` selects. The real-JS expectations live behind the `deno`
//! feature; the `Fake` backend (no-default-features contract) only has to
//! satisfy lifecycle/identity guarantees, so backend-independent structural
//! tests are kept separate from JS-semantics tests.

use std::time::Duration;
#[cfg(feature = "deno")]
use std::time::Instant;

use denosanbox::{
    BackendKind, Engine, EngineConfig, EngineLimits, InvocationOptions, PluginSource,
    SandboxOptions,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn fake_engine() -> Engine {
    Engine::new(EngineConfig::new(BackendKind::Fake)).expect("fake engine should be constructible")
}

fn call_defaults() -> InvocationOptions {
    InvocationOptions::default()
}

#[cfg(feature = "deno")]
fn with_timeout(ms: u64) -> InvocationOptions {
    InvocationOptions {
        timeout: Some(Duration::from_millis(ms)),
        deadline: None,
    }
}

fn counter_source() -> &'static str {
    r#"
let n = 0;
export function inc(_input) {
  n += 1;
  return n;
}
export async function incAsync(_input) {
  n += 1;
  return n;
}
export function current(_input) {
  return n;
}
"#
}

async fn prepared_counter(engine: &Engine) -> denosanbox::PreparedPlugin {
    engine
        .prepare(PluginSource::single("main.js", counter_source()))
        .expect("counter module should prepare")
}

/// Outer guard so a misbehaving backend cannot hang the test harness.
async fn within<F, T>(secs: u64, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(Duration::from_secs(secs), fut)
        .await
        .unwrap_or_else(|_| panic!("operation exceeded the outer {secs}s guard"))
}

// ---------------------------------------------------------------------------
// Backend-independent structure / lifecycle contract
// ---------------------------------------------------------------------------

#[test]
fn engine_clone_and_prepared_plugin_clone_are_usable() {
    let engine = fake_engine();
    let plugin = engine
        .prepare(PluginSource::single(
            "main.js",
            "export function ping(_input) { return 1; }",
        ))
        .expect("prepare should succeed on fake backend");

    // PreparedPlugin is documented as Clone.
    let plugin2 = plugin.clone();

    // Engine is expected to be Clone and still usable.
    let engine2 = engine.clone();
    drop(engine);
    drop(engine2);
    drop(plugin2);
}

#[test]
fn default_engine_config_selects_a_real_backend() {
    assert_eq!(EngineConfig::default().backend, BackendKind::Deno);
}

#[tokio::test(flavor = "current_thread")]
async fn close_is_idempotent_and_reports_closed_cleanly() {
    let engine = fake_engine();
    let plugin = prepared_counter(&engine).await;
    let sandbox = within(3, engine.create(plugin, SandboxOptions::default()))
        .await
        .expect("create should succeed");

    let first = within(3, sandbox.close())
        .await
        .expect("first close should succeed");
    assert!(
        first.closed_cleanly,
        "an unused sandbox should close cleanly"
    );

    let second = within(3, sandbox.close()).await.expect("second close");
    assert_eq!(first, second, "repeated close must preserve its report");

    // Post-close calls are typed `closed`; actual rendering is not asserted.
    let err = sandbox
        .call("current", json!(null), call_defaults())
        .await
        .expect_err("call after close must fail");
    assert_eq!(
        err.kind(),
        "closed",
        "post-close invocation must report typed closed, got {err:?}"
    );

    within(3, engine.shutdown())
        .await
        .expect("shutdown should succeed");
}

#[tokio::test(flavor = "current_thread")]
async fn message_size_limit_is_enforced() {
    let limits = EngineLimits {
        max_message_bytes: 256,
        ..EngineLimits::default()
    };
    let engine = Engine::new(EngineConfig {
        backend: BackendKind::Fake,
        limits,
    })
    .expect("engine with tightened limits should construct");
    let plugin = prepared_counter(&engine).await;
    let sandbox = within(3, engine.create(plugin, SandboxOptions::default()))
        .await
        .expect("create should succeed");

    let oversized = "x".repeat(8 * 1024);
    let err = sandbox
        .call("inc", Value::String(oversized), call_defaults())
        .await
        .expect_err("oversized payload must be rejected");
    assert_eq!(
        err.kind(),
        "limit_exceeded",
        "oversized message should be limit_exceeded, got {err:?}"
    );

    within(3, sandbox.close()).await.expect("close");
    within(3, engine.shutdown()).await.expect("shutdown");
}

#[tokio::test(flavor = "current_thread")]
async fn handle_after_shutdown_is_typed_closed() {
    let engine = fake_engine();
    let plugin = prepared_counter(&engine).await;
    let sandbox = within(3, engine.create(plugin, SandboxOptions::default()))
        .await
        .expect("create should succeed");

    let report = within(3, engine.shutdown()).await.expect("shutdown");
    assert!(report.clean, "shutdown of an idle engine should be clean");

    // Whatever the backend does internally, the handle must not silently work.
    match sandbox.call("inc", json!(null), call_defaults()).await {
        Err(err) => assert!(
            matches!(err.kind(), "closed" | "cancelled"),
            "post-shutdown invocation should be typed, got {err:?}"
        ),
        Ok(value) => panic!("post-shutdown invocation returned {value:?} instead of failing"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_without_inflight_call_is_not_fatal() {
    // `cancel()` is synchronous and must be safe on an idle sandbox.
    let engine = fake_engine();
    let plugin = prepared_counter(&engine).await;
    let sandbox = within(3, engine.create(plugin, SandboxOptions::default()))
        .await
        .expect("create should succeed");

    sandbox.cancel();
    within(3, sandbox.close())
        .await
        .expect("close after cancel");
    within(3, engine.shutdown()).await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Real JS semantics — only meaningful with the Deno backend compiled in
// ---------------------------------------------------------------------------

#[cfg(feature = "deno")]
mod real_js {
    use super::*;

    fn js_engine() -> Engine {
        Engine::new(EngineConfig::default()).expect("real Deno engine should construct")
    }

    async fn sandbox_for(engine: &Engine, source: &str) -> denosanbox::SandboxHandle {
        let plugin = engine
            .prepare(PluginSource::single("main.js", source))
            .unwrap_or_else(|e| panic!("prepare failed for source: {e:?}"));
        within(3, engine.create(plugin, SandboxOptions::default()))
            .await
            .unwrap_or_else(|e| panic!("create failed: {e:?}"))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_sandboxes_have_independent_module_state() {
        let engine = js_engine();
        let plugin = prepared_counter(&engine).await;

        let a = within(3, engine.create(plugin.clone(), SandboxOptions::default()))
            .await
            .expect("create a");
        let b = within(3, engine.create(plugin.clone(), SandboxOptions::default()))
            .await
            .expect("create b");

        let (ra, rb) = tokio::join!(
            async {
                a.call("inc", json!(null), call_defaults()).await?;
                a.call("inc", json!(null), call_defaults()).await?;
                a.call("current", json!(null), call_defaults()).await
            },
            async {
                b.call("inc", json!(null), call_defaults()).await?;
                b.call("current", json!(null), call_defaults()).await
            }
        );

        assert_eq!(ra.expect("a calls"), json!(2), "sandbox A counter");
        assert_eq!(rb.expect("b calls"), json!(1), "sandbox B counter");

        within(3, a.close()).await.expect("close a");
        within(3, b.close()).await.expect("close b");
        within(3, engine.shutdown()).await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn two_engines_are_independent_across_shutdown() {
        let e1 = js_engine();
        let e2 = js_engine();
        let p1 = e1
            .prepare(PluginSource::single("main.js", counter_source()))
            .expect("prepare on e1");
        let p2 = e2
            .prepare(PluginSource::single("main.js", counter_source()))
            .expect("prepare on e2");

        let s1 = within(3, e1.create(p1, SandboxOptions::default()))
            .await
            .expect("create on e1");
        let s2 = within(3, e2.create(p2, SandboxOptions::default()))
            .await
            .expect("create on e2");

        assert_eq!(
            s2.call("inc", json!(null), call_defaults())
                .await
                .expect("e2 inc before shutdown"),
            json!(1)
        );

        within(3, e1.shutdown()).await.expect("shutdown e1");
        within(3, s1.close()).await.ok();

        // e2 must remain fully usable after e1 shut down.
        assert_eq!(
            s2.call("inc", json!(null), call_defaults())
                .await
                .expect("e2 inc after e1 shutdown"),
            json!(2)
        );
        within(3, s2.close()).await.expect("close s2");
        within(3, e2.shutdown()).await.expect("shutdown e2");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn static_import_outside_graph_is_denied() {
        let engine = js_engine();
        let source = r#"
import "https://example.invalid/not-in-graph.js";
export function ping(_input) { return "unreachable"; }
"#;
        // Either prepare or create may reject; both stages are acceptable.
        let prepared = engine.prepare(PluginSource::single("main.js", source));
        let plugin = match prepared {
            Err(err) => {
                assert!(
                    matches!(err.kind(), "module_denied" | "module_not_found"),
                    "prepare rejection should be a module error, got {err:?}"
                );
                return;
            }
            Ok(p) => p,
        };

        match within(3, engine.create(plugin, SandboxOptions::default())).await {
            Err(err) => assert!(
                matches!(err.kind(), "module_denied" | "module_not_found"),
                "create rejection should be a module error, got {err:?}"
            ),
            Ok(sandbox) => {
                let err = sandbox
                    .call("ping", json!(null), call_defaults())
                    .await
                    .expect_err("out-of-graph import must not resolve at call time either");
                assert!(
                    matches!(
                        err.kind(),
                        "module_denied" | "module_not_found" | "execution"
                    ),
                    "call should report a typed denial, got {err:?}"
                );
                within(3, sandbox.close()).await.ok();
            }
        }

        within(3, engine.shutdown()).await.ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn computed_dynamic_import_is_denied_during_call() {
        let engine = js_engine();
        let source = r#"
export async function load(input) {
  const specifier = String(input);
  const mod = await import(specifier);
  return typeof mod;
}
"#;
        let sandbox = sandbox_for(&engine, source).await;

        let result = within(
            3,
            sandbox.call(
                "load",
                json!("https://example.invalid/x.js"),
                call_defaults(),
            ),
        )
        .await;
        match result {
            Err(err) => assert!(
                matches!(
                    err.kind(),
                    "module_denied" | "module_not_found" | "execution"
                ),
                "dynamic import denial should be typed, got {err:?}"
            ),
            Ok(value) => panic!("denied dynamic import returned a successful result: {value}"),
        }

        within(3, sandbox.close()).await.expect("close");
        within(3, engine.shutdown()).await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn host_scope_apis_are_absent() {
        let engine = js_engine();
        let source = r#"
export function probe(_input) {
  return {
    deno: typeof Deno,
    process: typeof process,
    require: typeof require,
  };
}
"#;
        let sandbox = sandbox_for(&engine, source).await;

        let value = within(3, sandbox.call("probe", json!(null), call_defaults()))
            .await
            .expect("typeof probe must not throw");

        let obj = value.as_object().expect("probe should return an object");
        assert_eq!(
            obj.get("deno"),
            Some(&json!("undefined")),
            "Deno must be absent"
        );
        assert_eq!(
            obj.get("process"),
            Some(&json!("undefined")),
            "process must be absent"
        );
        assert_eq!(
            obj.get("require"),
            Some(&json!("undefined")),
            "require must be absent"
        );

        within(3, sandbox.close()).await.expect("close");
        within(3, engine.shutdown()).await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn infinite_loop_respects_invocation_timeout() {
        let engine = js_engine();
        let source = r#"
export function spin(_input) {
  while (true) {}
}
"#;
        let sandbox = sandbox_for(&engine, source).await;

        let started = Instant::now();
        let err = within(3, sandbox.call("spin", json!(null), with_timeout(100)))
            .await
            .expect_err("infinite loop must be terminated by the invocation timeout");
        assert!(
            matches!(err.kind(), "deadline_exceeded" | "cancelled" | "execution"),
            "timeout should be a typed termination, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timing out should not wait for the outer guard"
        );

        within(3, sandbox.close()).await.ok();
        within(3, engine.shutdown()).await.ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn never_settling_promise_respects_invocation_timeout() {
        let engine = js_engine();
        let source = r#"
export function hang(_input) {
  return new Promise(() => {});
}
"#;
        let sandbox = sandbox_for(&engine, source).await;

        let result = within(3, sandbox.call("hang", json!(null), with_timeout(100))).await;
        match result {
            Err(err) => assert!(
                matches!(err.kind(), "deadline_exceeded" | "cancelled" | "execution"),
                "unsettled promise should surface a typed failure, got {err:?}"
            ),
            Ok(value) => panic!("never-settling promise unexpectedly resolved: {value:?}"),
        }

        within(3, sandbox.close()).await.ok();
        within(3, engine.shutdown()).await.ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_one_sandbox_leaves_the_other_usable() {
        let engine = js_engine();
        let plugin = prepared_counter(&engine).await;

        let a = within(3, engine.create(plugin.clone(), SandboxOptions::default()))
            .await
            .expect("create a");
        let b = within(3, engine.create(plugin, SandboxOptions::default()))
            .await
            .expect("create b");

        let spin_source = r#"
export function spin(_input) {
  while (true) {}
}
"#;
        // Reuse engine A but with a spinning module: separate sandbox so the
        // counter plugin on B is untouched.
        let spin_plugin = engine
            .prepare(PluginSource::single("spin.js", spin_source))
            .expect("prepare spin");
        let spinner = within(3, engine.create(spin_plugin, SandboxOptions::default()))
            .await
            .expect("create spinner");

        let call = within(3, async {
            let (result, ()) =
                tokio::join!(spinner.call("spin", json!(null), call_defaults()), async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    spinner.cancel();
                },);
            result
        })
        .await;

        match call {
            Err(err) => assert!(
                matches!(
                    err.kind(),
                    "cancelled" | "deadline_exceeded" | "closed" | "execution"
                ),
                "cancelled call should be typed, got {err:?}"
            ),
            Ok(value) => panic!("cancelled call returned {value:?}"),
        }

        // B (and A) must remain usable after the cancellation.
        assert_eq!(
            b.call("inc", json!(null), call_defaults())
                .await
                .expect("b usable"),
            json!(1)
        );
        assert_eq!(
            a.call("inc", json!(null), call_defaults())
                .await
                .expect("a usable"),
            json!(1)
        );

        within(3, spinner.close()).await.ok();
        within(3, a.close()).await.ok();
        within(3, b.close()).await.ok();
        within(3, engine.shutdown()).await.ok();
    }
}
