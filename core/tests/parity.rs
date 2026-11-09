#![cfg(feature = "deno")]
use denosanbox::{Engine, EngineConfig, InvocationOptions, PluginSource, SandboxOptions};
use denosanbox_testkit::{fixtures, Expected};
#[tokio::test]
async fn shared_behavior_contract() {
    for fixture in fixtures() {
        let engine = Engine::new(EngineConfig::default()).unwrap();
        let plugin = engine
            .prepare(PluginSource::single(fixture.entrypoint, fixture.source))
            .unwrap();
        let a = engine
            .create(plugin.clone(), SandboxOptions::default())
            .await
            .unwrap();
        let b = engine
            .create(plugin, SandboxOptions::default())
            .await
            .unwrap();
        for step in fixture.steps {
            let sandbox = [&a, &b][step.sandbox];
            let result = sandbox
                .call(&step.export, step.input, InvocationOptions::default())
                .await;
            match step.expected {
                Expected::Ok { value } => assert_eq!(result.unwrap(), value, "{}", fixture.id),
                Expected::Err { code } => {
                    assert_eq!(result.unwrap_err().kind(), code, "{}", fixture.id)
                }
            }
        }
        assert!(engine.shutdown().await.unwrap().clean, "{}", fixture.id);
    }
}
