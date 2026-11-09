//! Actual-JS conformance for the trusted managed guest Worker API.
#![cfg(feature = "deno")]

use std::collections::BTreeMap;
use std::time::Duration;

use denosanbox::modules::CompatibilityProfile;
use denosanbox::{
    Engine, EngineConfig, EngineLimits, InvocationOptions, PluginSource, SandboxOptions,
};
use serde_json::{json, Value};

fn engine() -> Engine {
    let mut config = EngineConfig::default();
    config.limits = EngineLimits {
        max_active_sandboxes: 8,
        ..EngineLimits::default()
    };
    Engine::new(config).unwrap()
}

/// Browser `Worker` profile: two independent child isolates with private state
/// exchange parent messages.
const BROWSER_PARENT: &str = r#"
export async function run(input) {
  const a = new Worker('./counter.js', { type: 'module' });
  const b = new Worker('./counter.js', { type: 'module' });
  const first = [];
  const second = [];
  const [ra1, rb1] = await Promise.all([send(a), send(b)]);
  const [ra2, rb2] = await Promise.all([send(a), send(b)]);
  first.push(ra1, rb1);
  second.push(ra2, rb2);
  await a.terminate();
  await b.terminate();
  return { first, second, input };
  function send(worker) {
    return new Promise((resolve, reject) => {
      worker.onmessage = (event) => resolve(event.data);
      worker.onerror = (event) => reject(new Error(event.message));
      worker.postMessage({ n: 1 });
    });
  }
}
"#;

const COUNTER_CHILD: &str = r#"
let counter = 0;
onmessage = (event) => {
  counter += 1;
  postMessage({ counter, got: event.data.n });
};
"#;

fn browser_source() -> PluginSource {
    PluginSource {
        entrypoint: "parent.js".into(),
        modules: BTreeMap::from([
            ("parent.js".into(), BROWSER_PARENT.into()),
            ("counter.js".into(), COUNTER_CHILD.into()),
        ]),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn browser_workers_have_independent_state_and_parent_messages() {
    let engine = engine();
    let plugin = engine.prepare(browser_source()).unwrap();
    let root = engine
        .create(plugin, SandboxOptions::default())
        .await
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        root.call("run", json!({"x": 1}), InvocationOptions::default()),
    )
    .await
    .expect("bounded worker round trips")
    .expect("browser worker run");
    assert_eq!(
        result,
        json!({
            "first": [{"counter": 1, "got": 1}, {"counter": 1, "got": 1}],
            "second": [{"counter": 2, "got": 1}, {"counter": 2, "got": 1}],
            "input": {"x": 1},
        })
    );
    assert_eq!(engine.active_sandboxes(), 1, "children must be closed");
    assert!(root.close().await.unwrap().closed_cleanly);
    assert!(engine.shutdown().await.unwrap().clean);
}

fn node_source() -> PluginSource {
    PluginSource {
        entrypoint: "main.js".into(),
        modules: BTreeMap::from([
            (
                "main.js".into(),
                r#"
import { Worker, isMainThread } from 'node:worker_threads';
export async function run(input) {
  const worker = new Worker('./child.js');
  const reply = await new Promise((resolve, reject) => {
    worker.once('message', resolve);
    worker.once('error', reject);
    worker.postMessage({ hello: input.name });
  });
  const again = await new Promise((resolve) => {
    worker.once('message', resolve);
    worker.postMessage({ hello: 'again' });
  });
  await worker.terminate();
  return { isMainThread, reply, again };
}
"#
                .into(),
            ),
            (
                "child.js".into(),
                r#"
import { parentPort, isMainThread } from 'worker_threads';
let seen = 0;
parentPort.on('message', (value) => {
  seen += 1;
  parentPort.postMessage({ seen, echo: value.hello, child: isMainThread });
});
"#
                .into(),
            ),
        ]),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn node_worker_threads_under_approved_profile() {
    let engine = engine();
    let plugin = engine
        .prepare_with_profile(node_source(), CompatibilityProfile::PureJsNpm)
        .unwrap();
    assert_eq!(plugin.profile(), CompatibilityProfile::PureJsNpm);
    let root = engine
        .create(plugin, SandboxOptions::default())
        .await
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        root.call("run", json!({"name": "root"}), InvocationOptions::default()),
    )
    .await
    .expect("bounded node worker")
    .expect("node worker run");
    assert_eq!(
        result,
        json!({
            "isMainThread": true,
            "reply": {"seen": 1, "echo": "root", "child": false},
            "again": {"seen": 2, "echo": "again", "child": false},
        })
    );
    assert!(root.close().await.unwrap().closed_cleanly);
}

#[tokio::test(flavor = "current_thread")]
async fn denied_profile_and_forged_builtin_cannot_gain_worker_threads() {
    let engine = engine();
    // JavaScriptOnly: the aliases are rejected before any execution.
    for specifier in ["node:worker_threads", "worker_threads"] {
        let source = PluginSource::single(
            "main.js",
            format!("import * as wt from '{specifier}'; export const m = wt;"),
        );
        assert!(
            engine
                .prepare_with_profile(source.clone(), CompatibilityProfile::JavaScriptOnly)
                .is_err(),
            "{specifier} must be denied by JavaScriptOnly"
        );
        let error = engine.prepare(source).unwrap_err();
        assert!(matches!(
            error,
            denosanbox::Error::ModuleDenied { .. } | denosanbox::Error::UnsupportedLanguage { .. }
        ));
    }
    // A guest-supplied raw builtin file is rejected even with the profile.
    let mut forged = PluginSource::single(
        "main.js",
        "import { Worker } from 'node:worker_threads'; export const m = Worker;",
    );
    forged
        .modules
        .insert("__builtin__/worker_threads.js".into(), "export const Worker = 1;".into());
    assert!(engine
        .prepare_with_profile(forged, CompatibilityProfile::PureJsNpm)
        .is_err());

    // A computed import cannot reach the alias in the default profile either.
    let computed = PluginSource::single(
        "main.js",
        "export async function run() { return import('node:' + 'worker_threads'); }",
    );
    if let Ok(plugin) = engine.prepare(computed) {
        let sandbox = engine
            .create(plugin, SandboxOptions::default())
            .await
            .unwrap();
        let denied = sandbox
            .call("run", Value::Null, InvocationOptions::default())
            .await;
        assert!(denied.is_err(), "computed worker_threads import must fail");
        let _ = sandbox.close().await;
    }
    let _ = engine.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn parent_cancellation_closes_descendants_and_keeps_sibling_usable() {
    let engine = engine();
    let left = engine
        .create(engine.prepare(browser_source()).unwrap(), SandboxOptions::default())
        .await
        .unwrap();
    let right = engine
        .create(engine.prepare(browser_source()).unwrap(), SandboxOptions::default())
        .await
        .unwrap();
    let pending = {
        let left = left.clone();
        tokio::spawn(async move {
            left.call(
                "run",
                Value::Null,
                InvocationOptions {
                    timeout: Some(Duration::from_secs(10)),
                    deadline: None,
                },
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    left.cancel();
    let result = pending.await.unwrap();
    assert!(result.is_err());
    // Sibling root remains fully usable and can still spawn workers.
    let sibling = tokio::time::timeout(
        Duration::from_secs(20),
        right.call("run", json!({"x": 2}), InvocationOptions::default()),
    )
    .await
    .expect("sibling bounded")
    .expect("sibling still works");
    assert_eq!(sibling["input"], json!({"x": 2}));
    assert!(left.close().await.unwrap().closed_cleanly);
    assert!(right.close().await.unwrap().closed_cleanly);
    assert_eq!(engine.active_sandboxes(), 0, "no leaked descendants");
    assert!(engine.shutdown().await.unwrap().clean);
}

#[tokio::test(flavor = "current_thread")]
async fn child_entrypoint_outside_graph_is_typed() {
    let engine = engine();
    let mut source = PluginSource::single(
        "lib/main.js",
        r#"export async function run() {
            const worker = new Worker('../outside.js', { type: 'module' });
            await worker.terminate();
            return 'accepted';
        }"#,
    );
    source
        .modules
        .insert("outside.js".into(), "export const n = 1;".into());
    let root = engine
        .create(engine.prepare(source).unwrap(), SandboxOptions::default())
        .await
        .unwrap();
    // The graph contains `outside.js`, but an execution-time `../` specifier is
    // never resolved by the broker, so nothing is spawned.
    let result = root
        .call("run", Value::Null, InvocationOptions::default())
        .await;
    if let Ok(value) = result {
        assert_eq!(value, Value::Null, "escaped entrypoint must not spawn");
    }
    assert_eq!(engine.active_sandboxes(), 1);
    assert!(root.close().await.unwrap().closed_cleanly);
}

#[tokio::test(flavor = "current_thread")]
async fn classify_workers_cannot_claim_classic_semantics() {
    let engine = engine();
    let root = engine
        .create(
            engine
                .prepare(PluginSource::single(
                    "main.js",
                    r#"export function run() {
                        const results = [];
                        for (const make of [
                          () => new Worker('./x.js'),
                          () => new Worker('./x.js', { type: 'classic' }),
                          () => new Worker('./x.js', { type: 'module', workerData: {} }),
                        ]) {
                          try { make(); results.push('accepted'); }
                          catch (error) { results.push(error.name); }
                        }
                        return results;
                    }"#,
                ))
                .unwrap(),
            SandboxOptions::default(),
        )
        .await
        .unwrap();
    let result = root
        .call("run", Value::Null, InvocationOptions::default())
        .await
        .unwrap();
    assert_eq!(result, json!(["UnsupportedFeature", "UnsupportedFeature", "UnsupportedFeature"]));
    assert!(root.close().await.unwrap().closed_cleanly);
}

#[tokio::test(flavor = "current_thread")]
async fn terminate_during_pending_spawn_releases_admission() {
    let engine = engine();
    let source = PluginSource {
        entrypoint: "main.js".into(),
        modules: BTreeMap::from([
            (
                "main.js".into(),
                r#"export async function run() {
                    const worker = new Worker('./child.js', { type: 'module' });
                    await worker.terminate();
                    const again = new Worker('./child.js', { type: 'module' });
                    const reply = await new Promise((resolve, reject) => {
                      again.onmessage = (event) => resolve(event.data);
                      again.onerror = (event) => reject(new Error(event.message));
                      again.postMessage(1);
                    });
                    await again.terminate();
                    return reply;
                }"#
                .into(),
            ),
            (
                "child.js".into(),
                "onmessage = (e) => postMessage({ ok: e.data });".into(),
            ),
        ]),
    };
    let root = engine
        .create(engine.prepare(source).unwrap(), SandboxOptions::default())
        .await
        .unwrap();
    let reply = tokio::time::timeout(
        Duration::from_secs(20),
        root.call("run", Value::Null, InvocationOptions::default()),
    )
    .await
    .expect("bounded")
    .expect("terminate during startup then reuse");
    assert_eq!(reply, json!({"ok": 1}));
    assert!(root.close().await.unwrap().closed_cleanly);
    assert_eq!(engine.active_sandboxes(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_queue_and_non_json_messages_are_typed_errors() {
    let engine = engine();
    let root = engine
        .create(
            engine
                .prepare(PluginSource::single(
                    "main.js",
                    r#"export function run() {
                        const results = [];
                        const worker = new Worker('./child.js', { type: 'module' });
                        const attempt = (label, fn) => {
                          try { fn(); results.push(label + ':accepted'); }
                          catch (error) { results.push(label + ':' + error.name); }
                        };
                        attempt('json', () => worker.postMessage(() => 1));
                        attempt('transfer', () => worker.postMessage({}, [new ArrayBuffer(8)]));
                        attempt('shared', () => worker.postMessage(new SharedArrayBuffer(8)));
                        for (let i = 0; i < 64; i++) {
                          try { worker.postMessage({ i }); } catch (error) { results.push('queue:' + error.name); break; }
                        }
                        return results;
                    }"#,
                ))
                .unwrap(),
            SandboxOptions::default(),
        )
        .await
        .unwrap();
    let result = root
        .call("run", Value::Null, InvocationOptions::default())
        .await
        .unwrap();
    let items: Vec<&str> = result
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(items.contains(&"json:DataCloneError"));
    assert!(items.contains(&"transfer:DataCloneError"));
    assert!(items.contains(&"shared:DataCloneError"));
    assert!(items.contains(&"queue:QuotaExceededError"));
    assert!(root.close().await.unwrap().closed_cleanly);
    let _ = engine.shutdown().await;
}
