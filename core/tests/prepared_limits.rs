#![cfg(feature = "deno")]

use std::collections::BTreeMap;

use denosanbox::{
    BackendKind, Engine, EngineConfig, EngineLimits, Error, PluginSource, PreparedPlugin,
    SandboxOptions,
};

fn engine_with_limits(limits: EngineLimits) -> Engine {
    Engine::new(EngineConfig::new(BackendKind::Deno).with_limits(limits)).unwrap()
}

async fn assert_create_rejected(target: &Engine, plugin: PreparedPlugin, expected_limit: &str) {
    let result = target.create(plugin, SandboxOptions::default()).await;
    match result {
        Err(Error::LimitExceeded { limit, .. }) => {
            assert_eq!(limit, expected_limit);
        }
        Err(other) => panic!("expected {expected_limit} limit rejection, got {other:?}"),
        Ok(sandbox) => {
            let report = sandbox.close().await.unwrap();
            panic!(
                "target engine accepted plugin above {expected_limit}; cleanup report: {report:?}"
            );
        }
    }
    assert_eq!(target.active_sandboxes(), 0);
}

fn type_heavy_module(name: &str, fields: usize) -> String {
    let mut source = format!("export function {name}(value: {{\n");
    for index in 0..fields {
        source.push_str(&format!("  field_{index}: string;\n"));
    }
    source.push_str("}): number {\n  return 1;\n}\n");
    source
}

#[tokio::test]
async fn target_engine_rechecks_raw_javascript_module_bytes() {
    let source = format!(
        "export function value() {{ return 1; }}\n//{}\n",
        "x".repeat(256)
    );
    let prepared = engine_with_limits(EngineLimits {
        max_module_bytes: 4096,
        max_graph_bytes: 8192,
        ..EngineLimits::default()
    })
    .prepare(PluginSource::single("main.js", source))
    .unwrap();

    let target = engine_with_limits(EngineLimits {
        max_module_bytes: 128,
        max_graph_bytes: 8192,
        ..EngineLimits::default()
    });
    assert_create_rejected(&target, prepared, "max_module_bytes").await;
}

#[tokio::test]
async fn target_engine_rechecks_raw_typescript_module_bytes_after_erasure() {
    let source = type_heavy_module("payload", 128);
    let prepared = engine_with_limits(EngineLimits {
        max_module_bytes: 8192,
        max_graph_bytes: 16 * 1024,
        ..EngineLimits::default()
    })
    .prepare(PluginSource::single("main.ts", source))
    .unwrap();
    assert!(prepared.total_bytes() > 256);
    assert!(prepared.source("main.ts").unwrap().len() < 256);

    let target = engine_with_limits(EngineLimits {
        max_module_bytes: 256,
        max_graph_bytes: 16 * 1024,
        ..EngineLimits::default()
    });
    assert_create_rejected(&target, prepared, "max_module_bytes").await;
}

#[tokio::test]
async fn target_engine_rechecks_raw_typescript_graph_bytes_after_erasure() {
    let main =
        "import { payload } from './payload.ts';\nexport function run() { return payload({}); }\n";
    let payload = type_heavy_module("payload", 128);
    let prepared = engine_with_limits(EngineLimits {
        max_module_bytes: 8192,
        max_graph_bytes: 16 * 1024,
        ..EngineLimits::default()
    })
    .prepare(PluginSource {
        entrypoint: "main.ts".into(),
        modules: BTreeMap::from([
            ("main.ts".into(), main.into()),
            ("payload.ts".into(), payload),
        ]),
    })
    .unwrap();
    let emitted_bytes: usize = prepared
        .iter_modules()
        .map(|(_, source)| source.len())
        .sum();
    assert!(prepared.total_bytes() > 1024);
    assert!(emitted_bytes < 512);

    let target = engine_with_limits(EngineLimits {
        max_module_bytes: 8192,
        max_graph_bytes: 512,
        ..EngineLimits::default()
    });
    assert_create_rejected(&target, prepared, "max_graph_bytes").await;
}

#[tokio::test]
async fn target_engine_rechecks_prepared_module_count() {
    let prepared = engine_with_limits(EngineLimits {
        max_modules: 4,
        max_module_bytes: 4096,
        max_graph_bytes: 8192,
        ..EngineLimits::default()
    })
    .prepare(PluginSource {
        entrypoint: "main.js".into(),
        modules: BTreeMap::from([
            (
                "main.js".into(),
                "import { left } from './left.js'; import { right } from './right.js'; export const total = left + right;".into(),
            ),
            ("left.js".into(), "export const left = 1;".into()),
            ("right.js".into(), "export const right = 2;".into()),
        ]),
    })
    .unwrap();
    assert_eq!(prepared.module_count(), 3);

    let target = engine_with_limits(EngineLimits {
        max_modules: 2,
        max_module_bytes: 4096,
        max_graph_bytes: 8192,
        ..EngineLimits::default()
    });
    assert_create_rejected(&target, prepared, "max_modules").await;
}
