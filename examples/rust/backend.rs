//! End-to-end example: a small order service on the real backend.
//!
//! A host request enters a guest export, which calls one scoped business
//! capability (`orders.read`). The engine ceiling allows that capability; each
//! sandbox grant pins a trusted tenant scope, and the simulated async host
//! callback derives the tenant from that trusted scope, never from guest input.
//! Two independent sandboxes keep independent counters, then both close
//! explicitly and the engine shuts down.
//!
//! Run from the crate root with:
//!   cargo run --example backend

use denosanbox::policy::{Error as CallbackError, Result as CallbackResult};
use denosanbox::{
    Engine, EngineConfig, HostGrant, InvocationContext, InvocationOptions, PluginSource, Policy,
    SandboxHandle, SandboxOptions,
};
use serde_json::{json, Value};
use std::time::Duration;

const PLUGIN: &str = r#"
let count = 0;
export async function inc(value) {
  count += value;
  return count;
}
export function counter() {
  return count;
}
export async function readOrder(request) {
  const order = await sandbox.host.call('orders.read', request);
  return order;
}
"#;

const ORDERS_READ: &str = "orders.read";
/// Hard bound on the serialized request accepted by the callback.
const MAX_ORDER_INPUT: usize = 2048;
/// Hard bound on the serialized response returned to the guest.
const MAX_ORDER_OUTPUT: usize = 1024;
/// Simulated lookup latency of the (in-memory) order store.
const LOOKUP_LATENCY: Duration = Duration::from_millis(25);

/// Simulated async host service: no network, no database, in-memory data only.
///
/// Request data is untrusted. The tenant is always `ctx.scope()`, which the
/// engine bound to this sandbox's grant, so `tenantId`/`scope` fields in the
/// payload are ignored. The callback re-checks cancellation/deadline after its
/// await so cancelled or expired requests stop before producing output.
async fn read_order(ctx: InvocationContext, input: Vec<u8>) -> CallbackResult<Vec<u8>> {
    ctx.check_live()?;
    if input.len() > MAX_ORDER_INPUT {
        return Err(CallbackError::limit_exceeded(
            "orders.read: input exceeds limit",
        ));
    }
    let request: Value = serde_json::from_slice(&input)
        .map_err(|_| CallbackError::invalid_argument("orders.read: input is not JSON"))?;
    let order_id = request
        .get("orderId")
        .and_then(Value::as_u64)
        .filter(|id| (1..=1_000_000).contains(id))
        .ok_or_else(|| {
            CallbackError::invalid_argument("orders.read: orderId must be 1..=1000000")
        })?;

    tokio::time::sleep(LOOKUP_LATENCY).await;
    // Re-check after awaiting: the call may have been cancelled or expired.
    ctx.check_live()?;

    let response = json!({
        "tenantId": ctx.scope(),
        "orderId": order_id,
        "status": "confirmed",
    });
    let output = serde_json::to_vec(&response)
        .map_err(|_| CallbackError::limit_exceeded("orders.read: response"))?;
    if output.len() > MAX_ORDER_OUTPUT {
        return Err(CallbackError::limit_exceeded(
            "orders.read: output exceeds limit",
        ));
    }
    Ok(output)
}

async fn tenant_sandbox(
    engine: &Engine,
    ceiling: &Policy,
    orders_read: denosanbox::RegisteredCallback,
    tenant: &str,
) -> Result<SandboxHandle, Box<dyn std::error::Error>> {
    let plugin = engine.prepare(PluginSource::single("main.js", PLUGIN))?;
    let options = SandboxOptions {
        // The sandbox policy may not exceed the engine ceiling.
        policy: ceiling.clone(),
        // Trusted per-sandbox grant: the only authority for the tenant.
        host_grants: vec![HostGrant {
            callback: orders_read,
            scope: tenant.into(),
        }],
        ..Default::default()
    };
    Ok(engine.create(plugin, options).await?)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Engine-wide ceiling: the host may grant only `orders.read`.
    let ceiling = Policy::default().allow_capability(ORDERS_READ)?;
    let engine = Engine::with_policy(EngineConfig::default(), ceiling.clone())?;

    let orders_read =
        engine.register_host_async(ORDERS_READ, MAX_ORDER_INPUT, MAX_ORDER_OUTPUT, read_order)?;

    let first = tenant_sandbox(&engine, &ceiling, orders_read.clone(), "tenant-a").await?;
    let second = tenant_sandbox(&engine, &ceiling, orders_read, "tenant-b").await?;

    let options = InvocationOptions::default();

    // Two independent isolates: each keeps its own counter.
    let first_result = first.call("inc", json!(1), options.clone()).await?;
    println!("first, after first call: {first_result}");
    let first_again = first.call("inc", json!(1), options.clone()).await?;
    println!("first, after second call: {first_again}");
    let second_result = second.call("inc", json!(1), options.clone()).await?;
    println!("second, after first call: {second_result}");

    assert_eq!(first_result, json!(1), "first call on first sandbox");
    assert_eq!(first_again, json!(2), "second call on first sandbox");
    assert_eq!(second_result, json!(1), "second call on second sandbox");

    // The guest forges tenant-b; the trusted grant keeps the reply in tenant-a.
    let reply = first
        .call(
            "readOrder",
            json!({ "orderId": 42, "tenantId": "tenant-b", "scope": "tenant-b" }),
            options.clone(),
        )
        .await?;
    println!("first, orders.read with forged tenantId/scope: {reply}");
    assert_eq!(reply["tenantId"], "tenant-a", "scope comes from the grant");
    assert_eq!(reply["orderId"], json!(42));
    assert_eq!(reply["status"], "confirmed");

    // The second sandbox has its own grant, so its scope is tenant-b.
    let reply_b = second
        .call(
            "readOrder",
            json!({ "orderId": 7, "tenantId": "tenant-a" }),
            options.clone(),
        )
        .await?;
    assert_eq!(reply_b["tenantId"], "tenant-b", "per-sandbox grant scope");
    assert_eq!(
        second.call("counter", Value::Null, options.clone()).await?,
        second_result,
        "host calls do not disturb isolate state"
    );
    assert_eq!(
        first.call("counter", Value::Null, options.clone()).await?,
        first_again,
        "host calls do not disturb isolate state"
    );

    // Invalid input is rejected by the host and surfaces as a guest error.
    let bad_input = first
        .call("readOrder", json!({ "orderId": 0 }), options.clone())
        .await;
    println!("first, orders.read with orderId 0: {bad_input:?}");
    assert!(bad_input.is_err(), "orderId 0 must be rejected");

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

    assert!(first_close.closed_cleanly, "first sandbox closes cleanly");
    assert!(second_close.closed_cleanly, "second sandbox closes cleanly");
    assert!(shutdown.clean, "engine shuts down cleanly");

    Ok(())
}
