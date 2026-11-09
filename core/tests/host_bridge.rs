#![cfg(feature = "deno")]
use denosanbox::*;
use serde_json::{json, Value};
use std::time::Duration;
#[tokio::test]
async fn guest_async_callback_uses_trusted_scope_and_private_resource_view() {
    let policy = Policy::default()
        .allow_capability("echo")
        .unwrap()
        .allow_capability("env")
        .unwrap()
        .allow_read("/tmp")
        .unwrap()
        .allow_write("/tmp")
        .unwrap();
    let engine = Engine::with_policy(EngineConfig::default(), policy.clone()).unwrap();
    let callback=engine.register_host_async("echo",1024,1024,|context,input|async move{
        tokio::time::sleep(Duration::from_millis(5)).await;
        Ok(serde_json::to_vec(&json!({"scope":context.scope(),"input":serde_json::from_slice::<Value>(&input).unwrap()})).unwrap())
    }).unwrap();
    let plugin=engine.prepare(PluginSource::single("main.js",r#"
      export async function run(input){
        await sandbox.fs.writeTextFile('/tmp/value',input.value);
        sandbox.env.set('PRIVATE',input.value);
        const host=await sandbox.host.call('echo',input);
        return {host,text:await sandbox.fs.readTextFile('/tmp/value'),env:sandbox.env.get('PRIVATE')};
      }
      export function inspect(){return sandbox.env.get('PRIVATE');}
    "#)).unwrap();
    let options = SandboxOptions {
        policy: policy.clone(),
        host_grants: vec![HostGrant {
            callback,
            scope: "tenant-a".into(),
        }],
        ..Default::default()
    };
    let a = engine
        .create(plugin.clone(), options.clone())
        .await
        .unwrap();
    let b = engine.create(plugin, options).await.unwrap();
    let result = a
        .call(
            "run",
            json!({"value":"private-a","scope":"forged"}),
            InvocationOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(result["host"]["scope"], "tenant-a");
    assert_eq!(result["text"], "private-a");
    assert_eq!(result["env"], "private-a");
    assert_eq!(
        b.call("inspect", Value::Null, InvocationOptions::default())
            .await
            .unwrap(),
        Value::Null
    );
    assert!(!a.close().await.unwrap().forced_termination);
    b.close().await.unwrap();
}
#[tokio::test]
async fn noncooperative_host_work_remains_accounted_after_deadline() {
    let policy = Policy::default().allow_capability("slow").unwrap();
    let engine = Engine::with_policy(EngineConfig::default(), policy.clone()).unwrap();
    let callback = engine
        .register_host("slow", 1024, 1024, |_: &InvocationContext, _: &[u8]| {
            std::thread::sleep(Duration::from_millis(250));
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
    let result = sandbox
        .call(
            "run",
            Value::Null,
            InvocationOptions {
                timeout: Some(Duration::from_millis(30)),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(
        result,
        Err(Error::DeadlineExceeded | Error::Cancelled)
    ));
    let report = sandbox.close().await.unwrap();
    assert!(!report.closed_cleanly);
    assert!(report.pending_host_operations > 0);
    assert_eq!(engine.active_sandboxes(), 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(engine.active_sandboxes(), 0);
}
