use denosanbox_policy::*;
use std::time::Duration;

#[test]
fn private_host_grant_is_scoped_to_its_port() {
    let n = NetworkRules::deny_all()
        .allow("db.example", 443)
        .unwrap()
        .allow_private("db.example", 5432)
        .unwrap();
    let ips = ["10.0.0.1".parse().unwrap()];
    assert!(n.check("db.example", 5432, &ips).is_ok());
    assert!(n.check("db.example", 443, &ips).is_err());
}
#[test]
fn non_global_ipv6_requires_explicit_private_grant() {
    for ip in [
        "::127.0.0.1",
        "fec0::1",
        "64:ff9b::a00:1",
        "2001::1",
        "3fff::1",
    ] {
        assert!(requires_private_grant(ip.parse().unwrap()), "{ip}");
    }
}
#[test]
fn revoke_regrant_invalidates_old_request() {
    let mut reg = HostRegistry::default();
    let cb = reg
        .register(
            "orders.read",
            128,
            128,
            |_: &InvocationContext, i: &[u8]| Ok(i.to_vec()),
        )
        .unwrap();
    let id = SandboxId::mint();
    reg.grant(id, &cb, "old").unwrap();
    let ctx = InvocationContext::new(
        id,
        Deadline::after(Duration::from_secs(30)),
        CancellationToken::new(),
        "old",
    );
    let req = reg.begin(&ctx, &cb, b"data".to_vec()).unwrap();
    reg.revoke_grant(id, &cb);
    reg.grant(id, &cb, "new").unwrap();
    assert!(reg.accept_result(&req, b"late".to_vec()).is_err());
}
#[test]
fn cancelled_callback_result_cannot_be_accepted() {
    let mut reg = HostRegistry::default();
    let cb = reg
        .register(
            "orders.read",
            128,
            128,
            |_: &InvocationContext, i: &[u8]| Ok(i.to_vec()),
        )
        .unwrap();
    let id = SandboxId::mint();
    reg.grant(id, &cb, "orders").unwrap();
    let token = CancellationToken::new();
    let ctx = InvocationContext::new(
        id,
        Deadline::after(Duration::from_secs(30)),
        token.clone(),
        "orders",
    );
    let req = reg.begin(&ctx, &cb, b"data".to_vec()).unwrap();
    token.cancel();
    assert!(reg.accept_result(&req, b"late".to_vec()).is_err());
}
#[test]
fn closed_sandbox_cannot_be_regranted() {
    let mut reg = HostRegistry::default();
    let cb = reg
        .register(
            "orders.read",
            128,
            128,
            |_: &InvocationContext, i: &[u8]| Ok(i.to_vec()),
        )
        .unwrap();
    let id = SandboxId::mint();
    reg.grant(id, &cb, "orders").unwrap();
    reg.close_sandbox(id);
    assert!(reg.grant(id, &cb, "orders").is_err());
}
#[test]
fn lease_amount_does_not_multiply_by_depth() {
    let root = Budget::new(100);
    let child = root.child(100);
    assert_eq!(child.reserve(10).unwrap().amount(), 10);
}
#[test]
fn extreme_deadline_does_not_panic() {
    let _ = Deadline::after(Duration::MAX);
}

#[test]
fn transferred_handle_is_rejected_for_different_trusted_caller() {
    let mut registry = ResourceRegistry::default();
    let a = SandboxId::mint();
    let b = SandboxId::mint();
    let handle = registry
        .insert(
            a,
            ResourceSpec::File {
                path: VirtualPath::parse("/private").unwrap(),
                writable: true,
            },
        )
        .unwrap();
    assert!(registry.get_for(b, handle).is_err());
    assert!(registry.remove_for(b, handle).is_err());
    assert!(registry.get_for(a, handle).is_ok());
}

#[test]
fn async_dispatch_runs_after_registry_lock_is_released() {
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    let registry = Arc::new(Mutex::new(HostRegistry::default()));
    let inside = registry.clone();
    let callback = registry
        .lock()
        .unwrap()
        .register_async(
            "echo",
            128,
            128,
            move |ctx: InvocationContext, input: Vec<u8>| {
                let inside = inside.clone();
                async move {
                    assert_eq!(ctx.scope(), "records");
                    let _guard = inside
                        .try_lock()
                        .expect("registry lock held during callback");
                    Ok(input)
                }
            },
        )
        .unwrap();
    let id = SandboxId::mint();
    registry
        .lock()
        .unwrap()
        .grant(id, &callback, "records")
        .unwrap();
    let ctx = InvocationContext::new(
        id,
        Deadline::after(Duration::from_secs(10)),
        CancellationToken::new(),
        "records",
    );
    let dispatch = registry
        .lock()
        .unwrap()
        .prepare_dispatch(&ctx, &callback, b"value".to_vec())
        .unwrap();
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = std::pin::pin!(dispatch.run_async());
    let Poll::Ready(result) = future.as_mut().poll(&mut context) else {
        panic!("immediate callback pending")
    };
    let (request, output) = result.unwrap();
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .accept_result(&request, output)
            .unwrap(),
        b"value"
    );
}
#[test]
fn same_scope_regrant_still_invalidates_old_result() {
    let mut registry = HostRegistry::default();
    let cb = registry
        .register("echo", 128, 128, |_: &InvocationContext, input: &[u8]| {
            Ok(input.to_vec())
        })
        .unwrap();
    let id = SandboxId::mint();
    registry.grant(id, &cb, "records").unwrap();
    let ctx = InvocationContext::new(
        id,
        Deadline::after(Duration::from_secs(10)),
        CancellationToken::new(),
        "records",
    );
    let request = registry.begin(&ctx, &cb, vec![]).unwrap();
    registry.revoke_grant(id, &cb);
    registry.grant(id, &cb, "records").unwrap();
    assert!(registry.accept_result(&request, vec![]).is_err());
}
#[test]
fn child_deadline_never_extends_parent() {
    let parent = Deadline::after(Duration::from_millis(10));
    let child = parent.child_after(Duration::from_secs(100));
    assert!(child.remaining().unwrap() <= parent.remaining().unwrap() + Duration::from_millis(1));
}
#[test]
fn vfs_denies_metadata_after_narrowing_and_close() {
    let p = Policy::default()
        .allow_read("/tmp")
        .unwrap()
        .allow_write("/tmp")
        .unwrap();
    let mut view = ResourceView::new(SandboxId::mint(), p, ViewLimits::default());
    view.write("/tmp/private", b"secret").unwrap();
    assert!(view.exists("/tmp/private"));
    view.narrow_policy(&Policy::default()).unwrap();
    assert!(!view.exists("/tmp/private"));
    assert!(view.read("/tmp/private").is_err());
    assert!(view
        .narrow_policy(&Policy::default().allow_read("/").unwrap())
        .is_err());
    view.close();
    assert!(!view.exists("/tmp"));
}
#[test]
fn failed_vfs_write_does_not_create_partial_directories() {
    let p = Policy::default()
        .allow_read("/")
        .unwrap()
        .allow_write("/")
        .unwrap();
    let mut view = ResourceView::new(
        SandboxId::mint(),
        p,
        ViewLimits {
            max_dirs: 3,
            ..Default::default()
        },
    );
    assert!(view.write("/a/b/c/d/file", b"x").is_err());
    assert!(!view.exists("/a"));
    assert!(!view.exists("/a/b/c/d"));
    view.write("/file", b"x").unwrap();
    assert!(view.write("/file/child", b"x").is_err());
    assert!(view.mkdir("/file").is_err());
    assert!(view.write("/tmp", b"x").is_err());
}
