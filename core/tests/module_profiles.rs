use denosanbox::modules::{CompatibilityProfile, Package};
use denosanbox::PluginSource;
#[cfg(feature = "deno")]
use denosanbox::{Engine, EngineConfig, InvocationOptions, SandboxOptions};
#[cfg(feature = "deno")]
use serde_json::json;
use std::collections::BTreeMap;

#[cfg(feature = "deno")]
#[tokio::test]
async fn typescript_static_and_computed_imports_execute_in_isolated_instances() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    let mut source = PluginSource::single(
        "main.ts",
        r#"
        import { bump } from './state.ts';
        interface Request { n: number }
        export async function run(input: Request): Promise<number> {
            const path: string = './state.ts';
            const loaded = await import(path);
            return bump() + loaded.bump() + input.n;
        }
    "#,
    );
    source.modules.insert(
        "state.ts".into(),
        "let count: number = 0; export function bump(): number { return ++count; }".into(),
    );
    let plugin = engine.prepare(source).unwrap();
    let a = engine
        .create(plugin.clone(), SandboxOptions::default())
        .await
        .unwrap();
    let b = engine
        .create(plugin, SandboxOptions::default())
        .await
        .unwrap();
    assert_eq!(
        a.call("run", json!({"n": 10}), InvocationOptions::default())
            .await
            .unwrap(),
        json!(13)
    );
    assert_eq!(
        b.call("run", json!({"n": 10}), InvocationOptions::default())
            .await
            .unwrap(),
        json!(13)
    );
    assert!(engine.shutdown().await.unwrap().clean);
}

#[test]
fn verified_packages_reject_tampering_and_unapproved_profiles() {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../fixtures/npm/sources.json")).unwrap();
    let entry = &manifest[0];
    let mut package = Package {
        name: "dequal".into(),
        version: "2.0.3".into(),
        entrypoint: "index.js".into(),
        modules: BTreeMap::from([(
            "index.js".into(),
            include_str!("../../fixtures/npm/dequal.js").into(),
        )]),
        integrity: BTreeMap::from([("index.js".into(), entry["sha256"].as_str().unwrap().into())]),
        install_scripts: false,
        native_dependencies: false,
    };
    let prepared = package.prepare().unwrap();
    assert!(PluginSource::single("main.js", "export function run(){}")
        .add_package(&prepared)
        .is_err());
    package
        .modules
        .get_mut("index.js")
        .unwrap()
        .push_str("\n// tampered");
    assert!(package
        .prepare_with_profile(CompatibilityProfile::PureJsNpm)
        .is_err());
}

#[cfg(feature = "deno")]
#[tokio::test]
async fn real_vendored_npm_esm_fixtures_use_the_same_graph_boundary() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    let mut source = PluginSource::single(
        "main.js",
        r#"
        import { dequal } from './vendor/dequal/index.js';
        import { nanoid } from './vendor/nanoid/index.js';
        export function run(input) {
            return { equal: dequal(input.a, input.b), length: nanoid(12).length,
                process: typeof process, deno: typeof Deno, worker: typeof Worker };
        }
    "#,
    );
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../fixtures/npm/sources.json")).unwrap();
    for (index, code) in [
        include_str!("../../fixtures/npm/dequal.js"),
        include_str!("../../fixtures/npm/nanoid.js"),
    ]
    .into_iter()
    .enumerate()
    {
        let entry = &manifest[index];
        let package = Package {
            name: entry["name"].as_str().unwrap().into(),
            version: entry["version"].as_str().unwrap().into(),
            entrypoint: "index.js".into(),
            modules: BTreeMap::from([("index.js".into(), code.into())]),
            integrity: BTreeMap::from([(
                "index.js".into(),
                entry["sha256"].as_str().unwrap().into(),
            )]),
            install_scripts: false,
            native_dependencies: false,
        };
        source
            .add_package(
                &package
                    .prepare_with_profile(CompatibilityProfile::PureJsNpm)
                    .unwrap(),
            )
            .unwrap();
    }
    let sandbox = engine
        .create(engine.prepare(source).unwrap(), SandboxOptions::default())
        .await
        .unwrap();
    let result = sandbox
        .call(
            "run",
            json!({"a":{"items":[1,2]},"b":{"items":[1,2]}}),
            InvocationOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(result["equal"], true);
    assert_eq!(result["length"], 12);
    assert_eq!(result["process"], "undefined");
    assert_eq!(result["worker"], "undefined");
    assert!(engine.shutdown().await.unwrap().clean);
}
