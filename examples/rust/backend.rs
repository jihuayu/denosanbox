//! End-to-end example: two independent sandboxes running the real backend.
//!
//! Run from the crate root with:
//!   cargo run --example backend

use denosanbox::{Engine, EngineConfig, InvocationOptions, PluginSource, SandboxOptions};
use serde_json::json;

const COUNTER: &str = r#"
let count = 0;
export async function inc(value) {
  count += value;
  return count;
}
"#;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = Engine::new(EngineConfig::default())?;

    let plugin = engine.prepare(PluginSource::single("main.js", COUNTER))?;

    let first = engine
        .create(plugin.clone(), SandboxOptions::default())
        .await?;
    let second = engine.create(plugin, SandboxOptions::default()).await?;

    let options = InvocationOptions::default();

    let first_result = first.call("inc", json!(1), options.clone()).await?;
    println!("first, after first call: {first_result}");
    let first_again = first.call("inc", json!(1), options.clone()).await?;
    println!("first, after second call: {first_again}");
    let second_result = second.call("inc", json!(1), options.clone()).await?;
    println!("second, after first call: {second_result}");

    assert_eq!(first_result, json!(1), "first call on first sandbox");
    assert_eq!(first_again, json!(2), "second call on first sandbox");
    assert_eq!(second_result, json!(1), "first call on second sandbox");

    let first_close = first.close().await?;
    println!(
        "first close: clean={} completed={} cancelled={} forced={}",
        first_close.closed_cleanly,
        first_close.invocations_completed,
        first_close.invocations_cancelled,
        first_close.forced_termination
    );

    let second_close = second.close().await?;
    println!(
        "second close: clean={} completed={} cancelled={} forced={}",
        second_close.closed_cleanly,
        second_close.invocations_completed,
        second_close.invocations_cancelled,
        second_close.forced_termination
    );

    let shutdown = engine.shutdown().await?;
    println!(
        "shutdown: clean={} closed={} forced={}",
        shutdown.clean, shutdown.sandboxes_closed, shutdown.sandboxes_forced
    );

    assert!(first_close.closed_cleanly);
    assert!(second_close.closed_cleanly);
    assert!(shutdown.clean);

    Ok(())
}
