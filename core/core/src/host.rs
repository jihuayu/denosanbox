//! Trusted per-sandbox resource and capability bridge.
//!
//! Host callbacks execute on independent native threads. A non-cooperative
//! callback retains its context/accounting until it really ends; timeout and
//! cancellation discard its result, not its committed side effects.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::{Error, Result};
use denosanbox_policy::{
    CancellationToken, Deadline, HostRegistry, InvocationContext, Policy, RegisteredCallback,
    ResourceView, SandboxId, ViewLimits,
};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGrant {
    pub callback: RegisteredCallback,
    pub scope: String,
}

#[derive(Debug)]
pub struct HostContext {
    id: SandboxId,
    view: Mutex<ResourceView>,
    registry: Arc<Mutex<HostRegistry>>,
    grants: BTreeMap<String, HostGrant>,
    cancellation: CancellationToken,
    deadline: Mutex<Deadline>,
    outstanding: Arc<AtomicUsize>,
    max_host_calls: usize,
    max_message_bytes: usize,
    closed: AtomicBool,
}

fn policy_error(error: denosanbox_policy::Error) -> Error {
    match error.kind() {
        denosanbox_policy::ErrorKind::Cancelled => Error::Cancelled,
        denosanbox_policy::ErrorKind::DeadlineExceeded => Error::DeadlineExceeded,
        kind => Error::Policy {
            kind: format!("{kind:?}"),
            message: format!("sandbox policy: {kind:?}"),
        },
    }
}
fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| Error::Internal {
        message: "host context lock poisoned".into(),
    })
}

struct Outstanding(Arc<AtomicUsize>);
impl Drop for Outstanding {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl HostContext {
    pub fn new(
        policy: Policy,
        registry: Arc<Mutex<HostRegistry>>,
        grants: Vec<HostGrant>,
    ) -> Result<Arc<Self>> {
        let id = SandboxId::mint();
        let limits = policy.limits();
        let mut indexed = BTreeMap::new();
        for grant in grants {
            policy
                .check_capability(grant.callback.name())
                .map_err(policy_error)?;
            if indexed
                .insert(grant.callback.name().to_string(), grant)
                .is_some()
            {
                return Err(Error::InvalidConfiguration {
                    reason: "duplicate sandbox host grant".into(),
                });
            }
        }
        {
            let mut hosts = lock(&registry)?;
            for grant in indexed.values() {
                if let Err(error) = hosts.grant(id, &grant.callback, &grant.scope) {
                    hosts.close_sandbox(id);
                    return Err(policy_error(error));
                }
            }
        }
        let view_limits = ViewLimits {
            max_file_bytes: limits.max_message_bytes(),
            max_total_bytes: limits.heap_target_bytes().min(16 * 1024 * 1024),
            ..Default::default()
        };
        Ok(Arc::new(Self {
            id,
            view: Mutex::new(ResourceView::new(id, policy, view_limits)),
            registry,
            grants: indexed,
            cancellation: CancellationToken::new(),
            deadline: Mutex::new(Deadline::after(limits.deadline())),
            outstanding: Arc::new(AtomicUsize::new(0)),
            max_host_calls: limits.queue_depth().min(32),
            max_message_bytes: limits.max_message_bytes(),
            closed: AtomicBool::new(false),
        }))
    }
    pub fn id(&self) -> SandboxId {
        self.id
    }
    pub fn outstanding(&self) -> usize {
        self.outstanding.load(Ordering::Acquire)
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    pub fn policy(&self) -> Policy {
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .policy()
            .clone()
    }
    pub fn narrow_policy(&self, requested: &Policy) -> Result<()> {
        self.narrow(requested)
    }
    pub fn begin_invocation(&self, duration: Duration) -> Result<()> {
        self.live()?;
        if self.outstanding() != 0 {
            return Err(Error::LimitExceeded {
                limit: "unfinished_host_calls".into(),
                detail: "previous invocation still owns host work".into(),
            });
        }
        let ceiling = lock(&self.view)?.policy().limits().deadline();
        *lock(&self.deadline)? = Deadline::after(duration.min(ceiling));
        Ok(())
    }
    pub fn cancel(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancellation.cancel();
        if let Ok(mut hosts) = self.registry.lock() {
            hosts.close_sandbox(self.id);
        }
        if let Ok(mut view) = self.view.lock() {
            view.close();
        }
    }
    fn live(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        self.cancellation.check().map_err(policy_error)
    }
    pub fn narrow(&self, requested: &Policy) -> Result<()> {
        self.live()?;
        lock(&self.view)?
            .narrow_policy(requested)
            .map_err(policy_error)
    }
    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        self.live()?;
        lock(&self.view)?.read(path).map_err(policy_error)
    }
    pub fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        self.live()?;
        lock(&self.view)?.write(path, data).map_err(policy_error)
    }
    pub fn env_get(&self, name: &str) -> Result<Option<String>> {
        self.live()?;
        lock(&self.view)?.env_get(name).map_err(policy_error)
    }
    pub fn env_set(&self, name: &str, value: &str) -> Result<()> {
        self.live()?;
        lock(&self.view)?.env_set(name, value).map_err(policy_error)
    }
    pub fn set_cwd(&self, path: &str) -> Result<()> {
        self.live()?;
        lock(&self.view)?.set_cwd(path).map_err(policy_error)
    }
    pub fn cwd(&self) -> Result<String> {
        self.live()?;
        Ok(lock(&self.view)?.cwd().as_str().to_string())
    }

    pub async fn call(self: &Arc<Self>, name: String, input: Value) -> Result<Value> {
        self.live()?;
        lock(&self.view)?
            .policy()
            .check_capability(&name)
            .map_err(policy_error)?;
        let grant = self.grants.get(&name).ok_or_else(|| Error::Policy {
            kind: "Denied".into(),
            message: "host capability not granted".into(),
        })?;
        let input = serde_json::to_vec(&input).map_err(|_| Error::Execution {
            message: "invalid host input".into(),
        })?;
        if input.len() > self.max_message_bytes {
            return Err(Error::LimitExceeded {
                limit: "host_input_bytes".into(),
                detail: "host input exceeds message limit".into(),
            });
        }
        let deadline = *lock(&self.deadline)?;
        let context = InvocationContext::new(
            self.id,
            deadline,
            self.cancellation.clone(),
            grant.scope.clone(),
        );
        let dispatch = lock(&self.registry)?
            .prepare_dispatch(&context, &grant.callback, input)
            .map_err(policy_error)?;
        let mut active = self.outstanding.load(Ordering::Acquire);
        loop {
            if active >= self.max_host_calls {
                return Err(Error::QueueFull {
                    capacity: self.max_host_calls,
                });
            }
            match self.outstanding.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => active = actual,
            }
        }
        let guard = Outstanding(self.outstanding.clone());
        let owner = self.clone();
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("denosanbox-host-call".into())
            .spawn(move || {
                let _guard = guard;
                let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if dispatch.is_async() {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|_| Error::Backend {
                                message: "host callback runtime unavailable".into(),
                            })?;
                        runtime.block_on(dispatch.run_async()).map_err(policy_error)
                    } else {
                        dispatch.run().map_err(policy_error)
                    }
                }));
                let result = match execution {
                    Err(_) => Err(Error::Backend {
                        message: "host callback panicked".into(),
                    }),
                    Ok(result) => result.and_then(|(request, output)| {
                        if output.len() > owner.max_message_bytes {
                            return Err(Error::LimitExceeded {
                                limit: "host_output_bytes".into(),
                                detail: "host output exceeds message limit".into(),
                            });
                        }
                        let output = lock(&owner.registry)?
                            .accept_result(&request, output)
                            .map_err(policy_error)?;
                        serde_json::from_slice(&output).map_err(|_| Error::Execution {
                            message: "host callback returned invalid JSON".into(),
                        })
                    }),
                };
                drop(_guard);
                let _ = sender.send(result);
            })
            .map_err(|_| Error::Backend {
                message: "host callback thread unavailable".into(),
            })?;
        // Periodic cancellation checks avoid a lost-notification race; deadline
        // and cancellation remain independent of the callback's cooperation.
        loop {
            self.live()?;
            deadline.check().map_err(policy_error)?;
            tokio::select! {
                result=&mut receiver=>return result.map_err(|_|Error::Backend{message:"host callback worker exited".into()})?,
                _=tokio::time::sleep(Duration::from_millis(5))=>{}
            }
        }
    }
}
impl Drop for HostContext {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    #[test]
    fn async_callback_has_trusted_scope_and_releases_registry_lock() {
        let hosts = Arc::new(Mutex::new(HostRegistry::default()));
        let inner = hosts.clone();
        let cb = hosts
            .lock()
            .unwrap()
            .register_async(
                "orders.read",
                1024,
                1024,
                move |ctx: InvocationContext, input: Vec<u8>| {
                    let inner = inner.clone();
                    async move {
                        assert_eq!(ctx.scope(), "tenant-a");
                        assert!(inner.try_lock().is_ok());
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        Ok(input)
                    }
                },
            )
            .unwrap();
        let ctx = HostContext::new(
            Policy::default().allow_capability("orders.read").unwrap(),
            hosts,
            vec![HostGrant {
                callback: cb,
                scope: "tenant-a".into(),
            }],
        )
        .unwrap();
        ctx.begin_invocation(Duration::from_secs(1)).unwrap();
        assert_eq!(
            runtime()
                .block_on(ctx.call("orders.read".into(), json!({"tenant":"forged"})))
                .unwrap(),
            json!({"tenant":"forged"})
        );
        assert_eq!(ctx.outstanding(), 0);
        assert!(runtime()
            .block_on(ctx.call("orders.write".into(), json!(null)))
            .is_err());
        ctx.cancel();
        assert!(ctx.is_closed());
    }
    #[test]
    fn noncooperative_callback_remains_accounted_after_deadline() {
        let hosts = Arc::new(Mutex::new(HostRegistry::default()));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let finish_rx = Mutex::new(finish_rx);
        let cb = hosts
            .lock()
            .unwrap()
            .register(
                "blocking",
                128,
                128,
                move |_: &InvocationContext, _: &[u8]| {
                    started_tx.send(()).unwrap();
                    finish_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                    Ok(b"null".to_vec())
                },
            )
            .unwrap();
        let ctx = HostContext::new(
            Policy::default().allow_capability("blocking").unwrap(),
            hosts,
            vec![HostGrant {
                callback: cb,
                scope: "test".into(),
            }],
        )
        .unwrap();
        ctx.begin_invocation(Duration::from_millis(25)).unwrap();
        assert!(matches!(
            runtime().block_on(ctx.call("blocking".into(), json!(null))),
            Err(Error::DeadlineExceeded)
        ));
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(ctx.outstanding(), 1);
        ctx.cancel();
        assert_eq!(ctx.outstanding(), 1);
        finish_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while ctx.outstanding() != 0 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(ctx.outstanding(), 0);
    }
    #[test]
    fn resource_views_and_narrowing_stay_private() {
        let hosts = Arc::new(Mutex::new(HostRegistry::default()));
        let policy = Policy::default()
            .allow_read("/tmp")
            .unwrap()
            .allow_write("/tmp")
            .unwrap()
            .allow_capability("env")
            .unwrap();
        let a = HostContext::new(policy.clone(), hosts.clone(), vec![]).unwrap();
        let b = HostContext::new(policy, hosts, vec![]).unwrap();
        a.write("/tmp/a", b"private").unwrap();
        a.env_set("TZ", "sandbox-only").unwrap();
        assert!(b.read("/tmp/a").is_err());
        assert_eq!(b.env_get("TZ").unwrap(), None);
        a.narrow_policy(&Policy::default()).unwrap();
        assert!(a.read("/tmp/a").is_err());
    }
}
