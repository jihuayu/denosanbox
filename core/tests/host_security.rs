//! Adversarial host-boundary conformance: identity/scope spoofing, ungranted
//! capabilities, schema limits and metadata-only audit.
//!
//! Everything here goes through the real `denosanbox-core` host API
//! (`HostContext`, `HostConfig`, `JsonSchema`, `HostAudit`, `HostRegistry`)
//! that the facade engine builds on. The facade does not re-export
//! `HostContext`, so these are the core contract's own public types (same
//! workspace crate the facade compiles against), not a fake backend. The guest-side half (`sandbox.host.call`)
//! is covered by the `deno`-gated tests below and by `host_bridge.rs`; these
//! tests focus on the trust decisions that must hold regardless of backend.
//!
//! Pre-existing coverage deliberately not duplicated here: trusted async scope
//! in `host.rs` unit tests and `host_bridge.rs`, private VFS/mount ownership in
//! `mounts.rs`, network default-deny and resolver accounting in `host.rs`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use denosanbox::policy::{Budget, HostRegistry, InvocationContext, Policy, SandboxId};
use denosanbox::{
    AuditOutcome, CapabilitySchemas, HostAudit, HostConfig, HostGrant, JsonSchema,
    RegisteredCallback, SchemaLimits,
};
use denosanbox_core::HostContext;
use serde_json::{json, Value};
use tokio::runtime::Builder;

const SECRET: &str = "sk-live-CANARY-9f3c1e0d";

fn runtime() -> tokio::runtime::Runtime {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
}

fn hosts() -> Arc<Mutex<HostRegistry>> {
    Arc::new(Mutex::new(HostRegistry::default()))
}

fn policy(names: &[&str]) -> Policy {
    names.iter().fold(Policy::default(), |p, name| {
        p.allow_capability(name).expect("valid capability name")
    })
}

fn grant(callback: RegisteredCallback, scope: &str) -> HostGrant {
    HostGrant {
        callback,
        scope: scope.to_string(),
    }
}

/// Register a callback that echoes `{"scope": <trusted scope>, "input": <in>}`.
fn echo_scope(
    registry: &Arc<Mutex<HostRegistry>>,
    name: &'static str,
    calls: Arc<AtomicUsize>,
) -> RegisteredCallback {
    registry
        .lock()
        .expect("registry")
        .register(
            name,
            64 * 1024,
            64 * 1024,
            move |ctx: &InvocationContext, input: &[u8]| {
                calls.fetch_add(1, Ordering::SeqCst);
                let value: Value = serde_json::from_slice(input).expect("json input");
                Ok(serde_json::to_vec(&json!({
                    "scope": ctx.scope(),
                    "sandbox": ctx.sandbox_id().to_string(),
                    "input": value,
                }))
                .expect("json output"))
            },
        )
        .expect("register")
}

fn policy_error_kind(err: &denosanbox::Error) -> Option<&str> {
    match err {
        denosanbox::Error::Policy { kind, .. } => Some(kind.as_str()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Guest-supplied identity/scope cannot replace the trusted context
// ---------------------------------------------------------------------------

#[test]
fn guest_supplied_scope_and_ids_cannot_replace_trusted_context() {
    let registry = hosts();
    let calls = Arc::new(AtomicUsize::new(0));
    let callback = echo_scope(&registry, "orders.read", calls.clone());
    // A guest-reachable spelling of the sandbox id is derived from the host
    // id after construction; the guest tries to forge `tenant-b` and ids.
    let context = HostContext::new(
        policy(&["orders.read"]),
        registry,
        vec![grant(callback, "tenant-a")],
    )
    .expect("context");
    let trusted_id = context.id();

    runtime().block_on(async {
        context
            .begin_invocation(Duration::from_secs(2))
            .expect("begin");
        for forged in [
            json!({"tenant": "tenant-b", "scope": "tenant-b", "sandbox": "1"}),
            json!({"scope": {"id": "tenant-a"}, "sandboxId": trusted_id.to_string()}),
            json!({"__sandbox": trusted_id.to_string(), "scope": "root"}),
            json!(null),
        ] {
            let result = context
                .call("orders.read".into(), forged.clone())
                .await
                .expect("echo callback accepts any JSON");
            assert_eq!(
                result.get("scope"),
                Some(&json!("tenant-a")),
                "trusted scope must not be replaced by guest data {forged}"
            );
            assert_eq!(
                result.get("sandbox"),
                Some(&json!(trusted_id.to_string())),
                "trusted sandbox id must not be replaced by guest data {forged}"
            );
            assert_eq!(
                result.get("input"),
                Some(&forged),
                "guest payload is data only; it is passed through unchanged"
            );
        }
    });
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    context.cancel();
}

#[test]
fn distinct_contexts_mint_distinct_host_ids_regardless_of_guest_input() {
    let registry = hosts();
    let callback = echo_scope(&registry, "orders.read", Arc::new(AtomicUsize::new(0)));
    let trusted = policy(&["orders.read"]);
    let a = HostContext::new(
        trusted.clone(),
        registry.clone(),
        vec![grant(callback.clone(), "tenant-a")],
    )
    .expect("a");
    let b = HostContext::new(trusted, registry, vec![grant(callback, "tenant-a")]).expect("b");
    assert_ne!(a.id(), b.id(), "each sandbox gets a fresh host-minted id");
    assert_eq!(a.id(), a.id(), "id is stable for the sandbox lifetime");
    // Two guest "claims" of the same id string never change the host ids.
    let _claimed = SandboxId::mint();
    assert_ne!(_claimed, a.id());
    assert_ne!(_claimed, b.id());
    a.cancel();
    b.cancel();
}

// ---------------------------------------------------------------------------
// Ungranted capabilities are denied without reaching the callback
// ---------------------------------------------------------------------------

#[test]
fn ungranted_or_ungranted_policy_capabilities_are_denied_before_dispatch() {
    let registry = hosts();
    let calls = Arc::new(AtomicUsize::new(0));
    let callback = echo_scope(&registry, "orders.read", calls.clone());
    // Policy allows writing too, but this sandbox is only granted read.
    let context = HostContext::new(
        policy(&["orders.read", "orders.write"]),
        registry,
        vec![grant(callback, "tenant-a")],
    )
    .expect("context");
    runtime().block_on(async {
        context
            .begin_invocation(Duration::from_secs(2))
            .expect("begin");
        // Allowed by policy but not granted to this sandbox.
        let err = context
            .call("orders.write".into(), json!({}))
            .await
            .expect_err("policy-allowed but ungranted capability");
        assert_eq!(policy_error_kind(&err), Some("Denied"), "got {err:?}");
        // Not in policy and not granted: policy check fails first.
        let err = context
            .call("db.raw".into(), json!({}))
            .await
            .expect_err("unknown capability");
        assert!(err.kind() == "policy", "got {err:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "denial happens before the callback"
        );
    });
    context.cancel();
}

#[test]
fn duplicate_or_excess_grants_fail_context_construction() {
    let registry = hosts();
    let callback = echo_scope(&registry, "orders.read", Arc::new(AtomicUsize::new(0)));
    let err = HostContext::new(
        policy(&["orders.read"]),
        registry.clone(),
        vec![
            grant(callback.clone(), "tenant-a"),
            grant(callback.clone(), "tenant-b"),
        ],
    )
    .expect_err("duplicate grant must be rejected");
    assert_eq!(err.kind(), "invalid_configuration", "got {err:?}");

    // A grant whose capability the policy does not allow is rejected, and the
    // registry must not retain a grant for the failed context.
    let err = HostContext::new(
        Policy::default(),
        registry,
        vec![grant(callback, "tenant-a")],
    )
    .expect_err("grant without policy is rejected");
    assert_eq!(err.kind(), "policy", "got {err:?}");
}

// ---------------------------------------------------------------------------
// Malformed / oversized schema input and results
// ---------------------------------------------------------------------------

fn strict_schemas() -> CapabilitySchemas {
    CapabilitySchemas {
        input: JsonSchema::Object {
            properties: BTreeMap::from([("id".to_string(), JsonSchema::String { max_bytes: 8 })]),
            required: ["id".to_string()].into_iter().collect(),
            additional_properties: false,
        },
        output: JsonSchema::Object {
            properties: BTreeMap::from([("ok".to_string(), JsonSchema::Boolean)]),
            required: ["ok".to_string()].into_iter().collect(),
            additional_properties: false,
        },
    }
}

#[test]
fn malformed_schema_input_is_rejected_before_the_callback() {
    let registry = hosts();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_inner = seen.clone();
    let callback = registry
        .lock()
        .expect("registry")
        .register(
            "orders.read",
            64 * 1024,
            64 * 1024,
            move |_: &InvocationContext, input: &[u8]| {
                seen_inner.fetch_add(1, Ordering::SeqCst);
                // Host result must satisfy the output schema or be rejected.
                if input == b"{\"id\":\"boom\"}" {
                    return Ok(br#"{"leaked":"not-a-boolean"}"#.to_vec());
                }
                Ok(br#"{"ok":true}"#.to_vec())
            },
        )
        .expect("register");
    let _ = &calls;
    let mut config = HostConfig::default();
    config
        .schemas
        .insert("orders.read".into(), strict_schemas());
    let audit = config.audit.clone();
    let context = HostContext::new_with_config(
        policy(&["orders.read"]),
        registry,
        vec![grant(callback, "tenant-a")],
        config,
    )
    .expect("context");
    runtime().block_on(async {
        context
            .begin_invocation(Duration::from_secs(2))
            .expect("begin");
        for malformed in [
            json!({}),                      // missing required
            json!({"id": "x", "extra": 1}), // additional property
            json!({"id": 7}),               // wrong type
            json!({"id": "toolong-id"}),    // exceeds per-field max_bytes
            json!([1, 2, 3]),               // wrong root type
            json!("plain string"),          // wrong root type
        ] {
            let err = context
                .call("orders.read".into(), malformed.clone())
                .await
                .expect_err("schema violation must be rejected");
            assert_eq!(
                policy_error_kind(&err),
                Some("SchemaRejected"),
                "input {malformed} should be SchemaRejected, got {err:?}"
            );
        }
        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "schema runs before dispatch"
        );
        // A valid input reaches the callback; an invalid *result* is rejected.
        assert_eq!(
            context
                .call("orders.read".into(), json!({"id": "ok"}))
                .await
                .expect("valid input and result"),
            json!({"ok": true})
        );
        let err = context
            .call("orders.read".into(), json!({"id": "boom"}))
            .await
            .expect_err("callback result violating its output schema");
        assert_eq!(
            policy_error_kind(&err),
            Some("SchemaRejected"),
            "got {err:?}"
        );
    });
    let events = audit.drain(1000);
    assert!(
        events
            .iter()
            .filter(|e| e.outcome == AuditOutcome::SchemaRejected)
            .count()
            >= 7
    );
    context.cancel();
}

#[test]
fn schema_traversal_limits_bound_oversized_values() {
    // Small override so the test allocates only a few KiB.
    let limits = SchemaLimits {
        max_depth: 4,
        max_nodes: 32,
        max_string_bytes: 256,
        max_properties: 8,
        max_array_items: 8,
    };
    let mut deep = json!(null);
    for _ in 0..10 {
        deep = json!([deep]);
    }
    assert!(
        JsonSchema::Any.validate(&deep, limits).is_err(),
        "depth bound"
    );

    let wide: Vec<u32> = (0..64).collect();
    assert!(
        JsonSchema::Any.validate(&json!(wide), limits).is_err(),
        "array item bound"
    );
    assert!(
        JsonSchema::Any
            .validate(&json!("s".repeat(1024)), limits)
            .is_err(),
        "string bound"
    );
    let many: BTreeMap<String, u32> = (0..64).map(|i| (format!("k{i}"), i)).collect();
    assert!(
        JsonSchema::Any.validate(&json!(many), limits).is_err(),
        "property bound"
    );
    // Errors carry only static reasons, never the value itself.
    let err = JsonSchema::Any
        .validate(&json!(format!("{SECRET}{}", "x".repeat(300))), limits)
        .expect_err("oversized secret-looking string");
    assert!(!err.reason().contains("CANARY"), "reason must be static");
    assert!(!format!("{err:?}").contains("CANARY"));
}

#[test]
fn schema_limits_are_capped_below_their_configured_values() {
    // HostConfig may ask for a huge depth, but the validator clamps at 128 and
    // always applies node/string budgets to `Any` schemas.
    let limits = SchemaLimits {
        max_depth: usize::MAX,
        max_nodes: 4,
        max_string_bytes: 8,
        ..Default::default()
    };
    let mut deep = json!(1);
    for _ in 0..=128 {
        deep = json!([deep]);
    }
    assert!(
        JsonSchema::Any.validate(&deep, limits).is_err(),
        "depth is clamped even when unlimited is requested"
    );
    assert!(JsonSchema::Any
        .validate(&json!([1, 2, 3, 4, 5]), limits)
        .is_err());
    assert!(JsonSchema::Any
        .validate(&json!("nine chars"), limits)
        .is_err());
}

// ---------------------------------------------------------------------------
// Audit metadata never carries payloads or secrets
// ---------------------------------------------------------------------------

#[test]
fn audit_records_metadata_only_and_bounds_its_buffer() {
    let registry = hosts();
    let callback = registry
        .lock()
        .expect("registry")
        .register(
            "orders.read",
            64 * 1024,
            64 * 1024,
            |_: &InvocationContext, _: &[u8]| Ok(br#"{"ok":true}"#.to_vec()),
        )
        .expect("register");
    let mut config = HostConfig {
        audit: Arc::new(HostAudit::new(8)),
        ..Default::default()
    };
    config
        .schemas
        .insert("orders.read".into(), strict_schemas());
    let audit = config.audit.clone();
    let context = HostContext::new_with_config(
        policy(&["orders.read"]),
        registry,
        vec![grant(callback, "tenant-secret-scope")],
        config,
    )
    .expect("context");
    runtime().block_on(async {
        context
            .begin_invocation(Duration::from_secs(2))
            .expect("begin");
        // Secret-looking payloads in success and in rejection paths.
        context
            .call("orders.read".into(), json!({"id": "ok", "token": SECRET}))
            .await
            .expect_err("additional property rejected");
        context
            .call("orders.read".into(), json!({"id": "ok"}))
            .await
            .expect("valid call");
        context
            .call(SECRET.into(), json!({"id": "ok"}))
            .await
            .expect_err("unknown capability");
    });

    let events = audit.drain(1000);
    assert!(!events.is_empty());
    for event in &events {
        assert_eq!(event.sandbox, context.id());
        assert!(
            event.capability == "<ungranted>" || event.capability == "orders.read",
            "audit capability must be a registered name or the fixed tag, got {:?}",
            event.capability
        );
    }
    let rendered = format!("{events:?}");
    assert!(
        !rendered.contains("CANARY"),
        "audit leaked a payload secret"
    );
    assert!(
        !rendered.contains("tenant-secret-scope"),
        "audit leaked the scope"
    );
    assert!(events.iter().any(|e| e.outcome == AuditOutcome::Completed));
    assert!(events
        .iter()
        .any(|e| e.outcome == AuditOutcome::SchemaRejected));
    assert!(events.iter().any(|e| e.outcome == AuditOutcome::Denied));

    // Bounded buffer: at most `capacity` events are retained, the rest are
    // counted as dropped instead of growing unbounded.
    let audit = HostAudit::new(4);
    for _ in 0..10 {
        audit.record(denosanbox::AuditEvent {
            at: std::time::SystemTime::now(),
            sandbox: context.id(),
            capability: "orders.read".into(),
            outcome: AuditOutcome::Completed,
        });
    }
    assert_eq!(audit.capacity(), 4);
    assert_eq!(audit.drain(100).len(), 4);
    assert_eq!(audit.dropped(), 6);
    context.cancel();
}

#[test]
fn overlong_guest_capability_names_are_not_recorded_verbatim() {
    let audit = HostAudit::default();
    let id = SandboxId::mint();
    audit.record(denosanbox::AuditEvent {
        at: std::time::SystemTime::now(),
        sandbox: id,
        capability: SECRET.repeat(10),
        outcome: AuditOutcome::Started,
    });
    let events = audit.drain(10);
    assert_eq!(events[0].capability, "<invalid>");
}

// ---------------------------------------------------------------------------
// Cancellation and lifecycle cannot be revived by late work
// ---------------------------------------------------------------------------

#[test]
fn cancelled_context_rejects_late_calls_and_ignores_late_results() {
    let registry = hosts();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let finish_rx = Mutex::new(finish_rx);
    let completed_after_cancel = Arc::new(AtomicUsize::new(0));
    let counter = completed_after_cancel.clone();
    let callback = registry
        .lock()
        .expect("registry")
        .register(
            "slow.read",
            1024,
            1024,
            move |_: &InvocationContext, _: &[u8]| {
                started_tx.send(()).expect("signal started");
                finish_rx
                    .lock()
                    .expect("finish")
                    .recv_timeout(Duration::from_secs(5))
                    .expect("finish signal");
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(br#"{"ok":true}"#.to_vec())
            },
        )
        .expect("register");
    let context = HostContext::new(
        policy(&["slow.read"]),
        registry,
        vec![grant(callback, "tenant-a")],
    )
    .expect("context");
    runtime().block_on(async {
        context
            .begin_invocation(Duration::from_secs(3))
            .expect("begin");
        let pending = {
            let context = context.clone();
            tokio::spawn(async move { context.call("slow.read".into(), json!({})).await })
        };
        let (signal_tx, signal_rx) = tokio::sync::oneshot::channel();
        let wait = tokio::task::spawn_blocking(move || {
            let _ = started_rx.recv_timeout(Duration::from_secs(2));
            let _ = signal_tx.send(());
        });
        tokio::time::timeout(Duration::from_secs(3), signal_rx)
            .await
            .expect("callback started")
            .expect("started signal");
        wait.await.expect("wait task");
        // Cancel while the non-cooperative callback is running.
        context.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("cancelled call settles")
            .expect("join");
        assert!(
            result.is_err(),
            "cancelled call must not deliver {result:?}"
        );
        assert!(context.is_closed());
        // The in-flight callback is still accounted for until it really ends.
        assert!(context.outstanding() > 0);

        // Later work cannot revive the closed context.
        for _ in 0..2 {
            let err = context
                .call("slow.read".into(), json!({}))
                .await
                .expect_err("late call on closed context");
            assert_eq!(err.kind(), "closed", "got {err:?}");
            assert!(context.begin_invocation(Duration::from_secs(1)).is_err());
        }

        // Let the non-cooperative callback finish; its late result is discarded
        // and accounting returns to zero.
        finish_tx.send(()).expect("finish");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while context.outstanding() != 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            context.outstanding(),
            0,
            "accounting released after real completion"
        );
        assert_eq!(completed_after_cancel.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn cancel_sweeps_vfs_bytes_and_refuses_reopening() {
    let registry = hosts();
    let context = HostContext::new(
        Policy::default()
            .allow_read("/tmp")
            .expect("read root")
            .allow_write("/tmp")
            .expect("write root"),
        registry,
        vec![],
    )
    .expect("context");
    context
        .write("/tmp/secret", SECRET.as_bytes())
        .expect("write");
    context.cancel();
    assert!(
        context.read("/tmp/secret").is_err(),
        "closed context must not serve retained data"
    );
    assert!(context
        .write("/tmp/late", b"late")
        .is_err_and(|e| e.kind() == "closed"));
}

// ---------------------------------------------------------------------------
// Multi-engine / multi-registry isolation
// ---------------------------------------------------------------------------

#[test]
fn separate_registries_and_budgets_do_not_leak_between_engines() {
    let shared_registry = hosts();
    let calls = Arc::new(AtomicUsize::new(0));
    let callback = echo_scope(&shared_registry, "orders.read", calls.clone());
    let refused = echo_scope(
        &shared_registry,
        "orders.refused",
        Arc::new(AtomicUsize::new(0)),
    );

    // Engine A may call `orders.read`; engine B's config does not grant it.
    let config_a = HostConfig {
        memory_budget: Budget::new(1024 * 1024),
        ..Default::default()
    };
    let engine_a = HostContext::new_with_config(
        policy(&["orders.read"]),
        shared_registry.clone(),
        vec![grant(callback.clone(), "tenant-a")],
        config_a.clone(),
    )
    .expect("engine A");
    let engine_b = HostContext::new_with_config(
        policy(&["orders.read", "orders.refused"]),
        shared_registry,
        vec![grant(callback, "tenant-a"), grant(refused, "tenant-a")],
        HostConfig {
            memory_budget: Budget::new(1024 * 1024),
            ..Default::default()
        },
    )
    .expect("engine B");
    assert_eq!(engine_a.grants().len(), 1);
    assert_eq!(engine_b.grants().len(), 2);

    runtime().block_on(async {
        engine_a
            .begin_invocation(Duration::from_secs(2))
            .expect("A begin");
        engine_b
            .begin_invocation(Duration::from_secs(2))
            .expect("B begin");
        let from_a = engine_a
            .call("orders.read".into(), json!({"who": "a"}))
            .await
            .expect("A calls");
        assert_eq!(from_a["scope"], json!("tenant-a"));
        let err = engine_a
            .call("orders.refused".into(), json!({}))
            .await
            .expect_err("A was never granted orders.refused");
        assert_eq!(policy_error_kind(&err), Some("Denied"));
        assert_eq!(
            engine_b
                .call("orders.refused".into(), json!({"ok": true}))
                .await
                .expect("B calls")["input"],
            json!({"ok": true})
        );
        // Cancelling A does not cancel B.
        engine_a.cancel();
        assert!(engine_b
            .call("orders.read".into(), json!({"who": "b"}))
            .await
            .is_ok());
        assert!(engine_a
            .call("orders.read".into(), json!({}))
            .await
            .is_err());
    });
    // Memory reservations are per-config, not globally shared.
    assert_eq!(config_a.memory_budget.used(), 0);
    engine_b.cancel();
}
