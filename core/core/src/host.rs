//! Trusted per-sandbox resource and capability bridge.
//!
//! Host callbacks execute on independent native threads. A non-cooperative
//! callback retains its context/accounting until it really ends; timeout and
//! cancellation discard its result, not its committed side effects.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::host_audit::{AuditEvent, AuditOutcome, HostAudit};
use crate::host_schema::{JsonSchema, SchemaLimits};
use crate::{Error, Result};
use denosanbox_connectors::{
    ConnectorError, ConnectorLimits, HttpConnector, HttpRequest, HttpResponse,
};
use denosanbox_mounts::{MountAccess, MountTable};
use denosanbox_policy::{Budget, Lease, VirtualPath};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

pub type WorkerFuture = Pin<Box<dyn Future<Output = Result<Value>> + Send + 'static>>;
pub type WorkerHandler = Arc<dyn Fn(String, Value) -> WorkerFuture + Send + Sync>;
#[derive(Default)]
struct WorkerHandlerState(Option<WorkerHandler>);
impl std::fmt::Debug for WorkerHandlerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerHandlerState")
            .field("installed", &self.0.is_some())
            .finish()
    }
}

use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitySchemas {
    pub input: JsonSchema,
    pub output: JsonSchema,
}
impl Default for CapabilitySchemas {
    fn default() -> Self {
        Self {
            input: JsonSchema::Any,
            output: JsonSchema::Any,
        }
    }
}
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub schemas: BTreeMap<String, CapabilitySchemas>,
    pub schema_limits: SchemaLimits,
    pub audit: Arc<HostAudit>,
    /// Shared ancestor admission pool. Clone this config for sibling sandboxes.
    pub host_budget: Arc<Budget>,
    pub memory_budget: Arc<Budget>,
}
impl Default for HostConfig {
    fn default() -> Self {
        Self {
            schemas: BTreeMap::new(),
            schema_limits: SchemaLimits::default(),
            audit: Arc::new(HostAudit::default()),
            host_budget: Budget::new(32),
            memory_budget: Budget::new(256 * 1024 * 1024),
        }
    }
}

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
    config: HostConfig,
    host_budget: Arc<Budget>,
    memory_budget: Arc<Budget>,
    vfs_leases: Mutex<BTreeMap<String, Lease>>,
    mounts: Mutex<MountTable>,
    worker_handler: Mutex<WorkerHandlerState>,
    worker_depth: AtomicUsize,
    http: Mutex<Arc<HttpConnector>>,
}

fn network_error(error: ConnectorError) -> Error {
    match error {
        ConnectorError::Cancelled => Error::Cancelled,
        ConnectorError::DeadlineExceeded => Error::DeadlineExceeded,
        other => Error::Policy {
            kind: other.code().to_string(),
            message: format!("network operation: {}", other.code()),
        },
    }
}

fn mount_error(error: denosanbox_mounts::Error) -> Error {
    Error::Policy {
        kind: "MountedResource".into(),
        message: error.to_string(),
    }
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
        Self::new_with_config(policy, registry, grants, HostConfig::default())
    }
    pub fn new_with_config(
        policy: Policy,
        registry: Arc<Mutex<HostRegistry>>,
        grants: Vec<HostGrant>,
        config: HostConfig,
    ) -> Result<Arc<Self>> {
        let id = SandboxId::mint();
        let limits = policy.limits();
        let http = HttpConnector::new(ConnectorLimits {
            max_request_bytes: limits.max_message_bytes(),
            max_response_bytes: limits.max_message_bytes(),
            timeout: limits.deadline(),
            ..Default::default()
        })
        .map_err(network_error)?;

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
                    hosts.forget_sandbox(id);
                    return Err(policy_error(error));
                }
            }
        }
        let view_limits = ViewLimits {
            max_file_bytes: limits.max_message_bytes(),
            max_total_bytes: limits.heap_target_bytes().min(16 * 1024 * 1024),
            ..Default::default()
        };
        let host_budget = config
            .host_budget
            .child(limits.queue_depth().min(32) as u64);
        let memory_budget = config
            .memory_budget
            .child(limits.heap_target_bytes() as u64);
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
            config,
            host_budget,
            memory_budget,
            vfs_leases: Mutex::new(BTreeMap::new()),
            mounts: Mutex::new(MountTable::new(id, 16, limits.max_message_bytes())),
            worker_handler: Mutex::new(WorkerHandlerState::default()),
            worker_depth: AtomicUsize::new(0),
            http: Mutex::new(Arc::new(http)),
        }))
    }
    pub fn id(&self) -> SandboxId {
        self.id
    }
    pub fn outstanding(&self) -> usize {
        let pending = self
            .http
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_resolutions();
        self.outstanding
            .load(Ordering::Acquire)
            .saturating_add(pending)
    }
    pub fn set_worker_depth(&self, depth: usize) {
        self.worker_depth.store(depth, Ordering::Release);
    }
    pub fn worker_depth(&self) -> usize {
        self.worker_depth.load(Ordering::Acquire)
    }
    pub fn grants(&self) -> Vec<HostGrant> {
        self.grants.values().cloned().collect()
    }
    /// Child configs retain this node as their concurrency ancestor.
    pub fn host_config(&self) -> HostConfig {
        HostConfig {
            host_budget: self.host_budget.clone(),
            memory_budget: self.memory_budget.clone(),
            ..self.config.clone()
        }
    }
    pub fn memory_budget(&self) -> Arc<Budget> {
        self.memory_budget.clone()
    }
    pub fn audit(&self) -> Arc<HostAudit> {
        self.config.audit.clone()
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
    pub fn deadline(&self) -> Result<Deadline> {
        Ok(*lock(&self.deadline)?)
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
        if let Ok(mut leases) = self.vfs_leases.lock() {
            leases.clear();
        }
        // A filesystem call may be blocked on a remote disk. Do not wait for its
        // mount-table lock from the supervisor cancellation path.
        if let Ok(mut mounts) = self.mounts.try_lock() {
            let _ = mounts.close(self.id);
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
    pub fn mount_directory(
        &self,
        prefix: &str,
        host_path: &Path,
        access: MountAccess,
    ) -> Result<()> {
        let (_guard, _admission) = self.reserve_work()?;
        let result = lock(&self.mounts)?
            .add_mount(self.id, prefix, host_path, access)
            .map_err(mount_error);
        self.finish_resource(result)
    }
    pub fn inherit_mounts_from(&self, parent: &HostContext) -> Result<()> {
        let (_guard, _admission) = self.reserve_work()?;
        parent.live()?;
        let inherited = parent
            .mounts
            .try_lock()
            .map_err(|_| Error::LimitExceeded {
                limit: "mount_setup_busy".into(),
                detail: "parent mount setup in progress".into(),
            })?
            .clone_for(self.id)
            .map_err(mount_error)?;
        parent.live()?;
        *self.mounts.try_lock().map_err(|_| Error::LimitExceeded {
            limit: "mount_setup_busy".into(),
            detail: "child mount setup in progress".into(),
        })? = inherited;
        self.finish_resource(Ok(()))
    }
    fn finish_resource<T>(&self, result: Result<T>) -> Result<T> {
        if self.is_closed() {
            if let Ok(mut mounts) = self.mounts.try_lock() {
                let _ = mounts.close(self.id);
            }
        }
        self.live()?;
        result
    }
    fn resolved_path(&self, path: &str) -> Result<(String, Policy)> {
        let view = lock(&self.view)?;
        Ok((
            view.resolve(path)
                .map_err(policy_error)?
                .as_str()
                .to_string(),
            view.policy().clone(),
        ))
    }
    /// Synchronous trusted-host convenience. Guest operations use read_async.
    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        let (_guard, _admission) = self.reserve_work()?;
        self.finish_resource(self.read_inner(path))
    }
    fn read_inner(&self, path: &str) -> Result<Vec<u8>> {
        self.live()?;
        let (path, policy) = self.resolved_path(path)?;
        let mounted = {
            let mounts = lock(&self.mounts)?;
            if mounts.contains(&path) {
                Some(mounts.clone_for(self.id).map_err(mount_error)?)
            } else {
                None
            }
        };
        if let Some(mount) = mounted {
            return mount.read(self.id, &policy, &path).map_err(mount_error);
        }
        lock(&self.view)?.read(&path).map_err(policy_error)
    }
    /// Synchronous trusted-host convenience. Guest operations use write_async.
    pub fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        let (_guard, _admission) = self.reserve_work()?;
        self.finish_resource(self.write_inner(path, data))
    }
    fn write_inner(&self, path: &str, data: &[u8]) -> Result<()> {
        self.live()?;
        let (path, policy) = self.resolved_path(path)?;
        let mounted = {
            let mounts = lock(&self.mounts)?;
            if mounts.contains(&path) {
                Some(mounts.clone_for(self.id).map_err(mount_error)?)
            } else {
                None
            }
        };
        if let Some(mount) = mounted {
            return mount
                .write(self.id, &policy, &path, data)
                .map_err(mount_error);
        }
        let mut view = lock(&self.view)?;
        let mut leases = lock(&self.vfs_leases)?;
        // Charge the incoming Vec allocation before replacing the old one.
        let lease = self
            .memory_budget
            .reserve(data.len() as u64)
            .map_err(policy_error)?;
        view.write(&path, data).map_err(policy_error)?;
        leases.insert(path, lease);
        Ok(())
    }
    pub async fn read_async(self: &Arc<Self>, path: String) -> Result<Vec<u8>> {
        self.offload(move |context| context.read_inner(&path)).await
    }
    pub async fn write_async(self: &Arc<Self>, path: String, data: Vec<u8>) -> Result<()> {
        self.offload(move |context| context.write_inner(&path, &data))
            .await
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
        let (_guard, _admission) = self.reserve_work()?;
        self.finish_resource(self.set_cwd_inner(path))
    }
    fn set_cwd_inner(&self, path: &str) -> Result<()> {
        self.live()?;
        let (path, policy) = self.resolved_path(path)?;
        let mounted = {
            let mounts = lock(&self.mounts)?;
            if mounts.contains(&path) {
                Some(mounts.clone_for(self.id).map_err(mount_error)?)
            } else {
                None
            }
        };
        if let Some(mount) = mounted {
            mount.is_dir(self.id, &policy, &path).map_err(mount_error)?;
            return lock(&self.view)?
                .set_external_cwd(VirtualPath::parse(&path).map_err(policy_error)?)
                .map_err(policy_error);
        }
        lock(&self.view)?.set_cwd(&path).map_err(policy_error)
    }
    pub async fn set_cwd_async(self: &Arc<Self>, path: String) -> Result<()> {
        self.offload(move |context| context.set_cwd_inner(&path))
            .await
    }
    pub fn cwd(&self) -> Result<String> {
        self.live()?;
        Ok(lock(&self.view)?.cwd().as_str().to_string())
    }

    fn reserve_work(&self) -> Result<(Outstanding, Lease)> {
        self.live()?;
        let admission = self.host_budget.reserve(1).map_err(policy_error)?;
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
                Ok(_) => return Ok((Outstanding(self.outstanding.clone()), admission)),
                Err(actual) => active = actual,
            }
        }
    }
    async fn offload<T, F>(self: &Arc<Self>, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&HostContext) -> Result<T> + Send + 'static,
    {
        let deadline = self.deadline()?;
        deadline.check().map_err(policy_error)?;
        let (guard, admission) = self.reserve_work()?;
        let owner = self.clone();
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("denosanbox-resource-io".into())
            .spawn(move || {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&owner)))
                        .unwrap_or_else(|_| {
                            Err(Error::Backend {
                                message: "resource worker panicked".into(),
                            })
                        })
                        .and_then(|value| {
                            owner.live()?;
                            deadline.check().map_err(policy_error)?;
                            Ok(value)
                        });
                if owner.is_closed() {
                    if let Ok(mut mounts) = owner.mounts.try_lock() {
                        let _ = mounts.close(owner.id);
                    }
                }
                drop(admission);
                drop(guard);
                let _ = sender.send(result);
            })
            .map_err(|_| Error::Backend {
                message: "resource worker unavailable".into(),
            })?;
        loop {
            self.live()?;
            deadline.check().map_err(policy_error)?;
            tokio::select! {
                result=&mut receiver=>{self.live()?;deadline.check().map_err(policy_error)?;return result.map_err(|_|Error::Backend{message:"resource worker exited".into()})?;},
                _=tokio::time::sleep(Duration::from_millis(5))=>{}
            }
        }
    }
    /// Install the trusted facade broker once, before running guest initialization.
    pub fn set_worker_handler(&self, handler: WorkerHandler) -> Result<()> {
        self.live()?;
        let mut slot = lock(&self.worker_handler)?;
        if slot.0.is_some() {
            return Err(Error::InvalidConfiguration {
                reason: "worker broker already installed".into(),
            });
        }
        slot.0 = Some(handler);
        Ok(())
    }
    pub async fn worker_operation(
        self: &Arc<Self>,
        operation: String,
        input: Value,
    ) -> Result<Value> {
        self.live()?;
        let deadline = self.deadline()?;
        let handler =
            lock(&self.worker_handler)?
                .0
                .clone()
                .ok_or_else(|| Error::UnsupportedFeature {
                    feature: "managed_workers".into(),
                })?;
        let control = matches!(operation.as_str(), "close" | "cancel" | "terminate");
        let _admission = if control {
            None
        } else {
            Some(self.reserve_work()?)
        };
        let mut future = (handler)(operation, input);
        loop {
            self.live()?;
            deadline.check().map_err(policy_error)?;
            tokio::select! {
                result=&mut future=>{self.live()?;deadline.check().map_err(policy_error)?;return result;},
                _=tokio::time::sleep(Duration::from_millis(5))=>{}
            }
        }
    }
    /// Trusted host transport customization, e.g. a deterministic resolver.
    /// Guest input never selects or replaces the connector or resolver.
    pub fn set_http_connector(&self, connector: HttpConnector) -> Result<()> {
        self.live()?;
        if self.outstanding() != 0 {
            return Err(Error::InvalidConfiguration {
                reason: "cannot replace active HTTP connector".into(),
            });
        }
        *lock(&self.http)? = Arc::new(connector);
        Ok(())
    }
    pub async fn http_request(self: &Arc<Self>, request: HttpRequest) -> Result<HttpResponse> {
        let connector = lock(&self.http)?.clone();
        self.offload(move |context| {
            let remaining = context
                .deadline()?
                .remaining()
                .ok_or(Error::DeadlineExceeded)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| Error::Backend {
                    message: "network runtime unavailable".into(),
                })?;
            let result = runtime
                .block_on(connector.execute_with_policy(
                    request,
                    || context.policy().network().clone(),
                    &context.cancellation,
                    remaining,
                ))
                .map_err(network_error);
            drop(runtime);
            // getaddrinfo is non-cooperative. Retain this owner's native work,
            // ancestor admission and context until its resolver really returns.
            while connector.pending_resolutions() != 0 {
                std::thread::sleep(Duration::from_millis(5));
            }
            result
        })
        .await
    }
    pub fn protocol_version(&self) -> u16 {
        denosanbox_policy::PROTOCOL_VERSION
    }
    /// Explicit negotiation boundary for native guest ops and external bindings.
    pub async fn call_versioned(
        self: &Arc<Self>,
        version: u16,
        name: String,
        input: Value,
    ) -> Result<Value> {
        if version != self.protocol_version() {
            return Err(Error::Policy {
                kind: "Protocol".into(),
                message: "unsupported host capability protocol version".into(),
            });
        }
        self.call(name, input).await
    }
    pub async fn call(self: &Arc<Self>, name: String, input: Value) -> Result<Value> {
        let audit_name = self
            .grants
            .get(&name)
            .map_or("<ungranted>", |grant| grant.callback.name())
            .to_string();
        self.config.audit.record(AuditEvent {
            at: SystemTime::now(),
            sandbox: self.id,
            capability: audit_name.clone(),
            outcome: AuditOutcome::Started,
        });
        let result = self.call_inner(name, input).await;
        let outcome = match &result {
            Ok(_) => AuditOutcome::Completed,
            Err(Error::Cancelled | Error::Closed) => AuditOutcome::Cancelled,
            Err(Error::DeadlineExceeded) => AuditOutcome::DeadlineExceeded,
            Err(Error::QueueFull { .. } | Error::LimitExceeded { .. }) => {
                AuditOutcome::LimitExceeded
            }
            Err(Error::Policy { kind, .. }) if kind == "SchemaRejected" => {
                AuditOutcome::SchemaRejected
            }
            Err(Error::Policy { kind, .. }) if kind == "LimitExceeded" || kind == "Overflow" => {
                AuditOutcome::LimitExceeded
            }

            Err(Error::Policy { .. }) => AuditOutcome::Denied,
            Err(_) => AuditOutcome::Failed,
        };
        self.config.audit.record(AuditEvent {
            at: SystemTime::now(),
            sandbox: self.id,
            capability: audit_name,
            outcome,
        });
        result
    }
    async fn call_inner(self: &Arc<Self>, name: String, input: Value) -> Result<Value> {
        self.live()?;
        lock(&self.view)?
            .policy()
            .check_capability(&name)
            .map_err(policy_error)?;
        let grant = self.grants.get(&name).ok_or_else(|| Error::Policy {
            kind: "Denied".into(),
            message: "host capability not granted".into(),
        })?;
        let schemas = self.config.schemas.get(&name).cloned().unwrap_or_default();
        let schema_limits = SchemaLimits {
            max_string_bytes: self
                .config
                .schema_limits
                .max_string_bytes
                .min(self.max_message_bytes),
            ..self.config.schema_limits
        };
        schemas
            .input
            .validate(&input, schema_limits)
            .map_err(|_| Error::Policy {
                kind: "SchemaRejected".into(),
                message: "host input failed registered schema".into(),
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
        let admission = self.host_budget.reserve(1).map_err(policy_error)?;
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
                let _admission = admission;
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
                        owner.live()?;
                        deadline.check().map_err(policy_error)?;
                        if output.len() > owner.max_message_bytes {
                            return Err(Error::LimitExceeded {
                                limit: "host_output_bytes".into(),
                                detail: "host output exceeds message limit".into(),
                            });
                        }
                        let output = lock(&owner.registry)?
                            .accept_result(&request, output)
                            .map_err(policy_error)?;
                        let value: Value =
                            serde_json::from_slice(&output).map_err(|_| Error::Execution {
                                message: "host callback returned invalid JSON".into(),
                            })?;
                        schemas
                            .output
                            .validate(&value, schema_limits)
                            .map_err(|_| Error::Policy {
                                kind: "SchemaRejected".into(),
                                message: "host output failed registered schema".into(),
                            })?;
                        Ok(value)
                    }),
                };
                drop(_admission);
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
                result=&mut receiver=> {
                    self.live()?;
                    deadline.check().map_err(policy_error)?;
                    return result.map_err(|_|Error::Backend{message:"host callback worker exited".into()})?;
                },
                _=tokio::time::sleep(Duration::from_millis(5))=>{}
            }
        }
    }
}
impl Drop for HostContext {
    fn drop(&mut self) {
        self.cancel();
        // Every native resource worker holds an Arc<Self>, so this is later
        // than its real completion, including non-cooperative host/DNS work.
        if let Ok(mut registry) = self.registry.lock() {
            registry.forget_sandbox(self.id);
        }
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
    #[test]
    fn schemas_reject_before_callback_and_audit_never_records_payloads() {
        let hosts = Arc::new(Mutex::new(HostRegistry::default()));
        let count = Arc::new(AtomicUsize::new(0));
        let called = count.clone();
        let cb = hosts
            .lock()
            .unwrap()
            .register(
                "orders.read",
                1024,
                1024,
                move |_: &InvocationContext, _: &[u8]| {
                    called.fetch_add(1, Ordering::Relaxed);
                    Ok(br#""secret-output""#.to_vec())
                },
            )
            .unwrap();
        let mut config = HostConfig::default();
        config.schemas.insert(
            "orders.read".into(),
            CapabilitySchemas {
                input: JsonSchema::Integer,
                output: JsonSchema::Boolean,
            },
        );
        let audit = config.audit.clone();
        let ctx = HostContext::new_with_config(
            Policy::default().allow_capability("orders.read").unwrap(),
            hosts,
            vec![HostGrant {
                callback: cb,
                scope: "secret-scope".into(),
            }],
            config,
        )
        .unwrap();
        ctx.begin_invocation(Duration::from_secs(1)).unwrap();
        assert!(
            matches!(runtime().block_on(ctx.call("orders.read".into(),json!("secret-input"))),Err(Error::Policy{kind,..}) if kind=="SchemaRejected")
        );
        assert_eq!(count.load(Ordering::Relaxed), 0);
        assert!(
            matches!(runtime().block_on(ctx.call("orders.read".into(),json!(1))),Err(Error::Policy{kind,..}) if kind=="SchemaRejected")
        );
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert!(runtime()
            .block_on(ctx.call("secret-capability-name".into(), json!(null)))
            .is_err());
        let events = audit.drain(100);
        assert!(events
            .iter()
            .any(|e| e.outcome == AuditOutcome::SchemaRejected));
        assert!(!format!("{events:?}").contains("secret"));
    }
    #[test]
    fn child_host_calls_share_ancestor_admission_until_actual_completion() {
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
        let policy = Policy::default().allow_capability("blocking").unwrap();
        let config = HostConfig {
            host_budget: Budget::new(1),
            ..Default::default()
        };
        let global = config.host_budget.clone();
        let grant = HostGrant {
            callback: cb,
            scope: "test".into(),
        };
        let parent = HostContext::new_with_config(
            policy.clone(),
            hosts.clone(),
            vec![grant.clone()],
            config,
        )
        .unwrap();
        let child =
            HostContext::new_with_config(policy, hosts, vec![grant], parent.host_config()).unwrap();
        parent.begin_invocation(Duration::from_secs(2)).unwrap();
        child.begin_invocation(Duration::from_secs(2)).unwrap();
        runtime().block_on(async {
            let first=tokio::spawn(async move{parent.call("blocking".into(),json!(null)).await});
            let deadline=std::time::Instant::now()+Duration::from_secs(1);
            while started_rx.try_recv().is_err() {
                assert!(std::time::Instant::now()<deadline,"callback did not start");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert_eq!(global.used(),1);
            assert!(matches!(child.call("blocking".into(),json!(null)).await,Err(Error::Policy{kind,..}) if kind=="LimitExceeded"));
            finish_tx.send(()).unwrap();
            assert_eq!(first.await.unwrap().unwrap(),json!(null));
            assert_eq!(global.used(),0);
        });
    }
    #[test]
    fn private_vfs_bytes_share_ancestor_memory_and_release_on_close() {
        let hosts = Arc::new(Mutex::new(HostRegistry::default()));
        let config = HostConfig {
            memory_budget: Budget::new(5),
            ..Default::default()
        };
        let global = config.memory_budget.clone();
        let policy = Policy::default()
            .allow_read("/tmp")
            .unwrap()
            .allow_write("/tmp")
            .unwrap();
        let a = HostContext::new_with_config(policy.clone(), hosts.clone(), vec![], config.clone())
            .unwrap();
        let b = HostContext::new_with_config(policy, hosts, vec![], config).unwrap();
        a.write("/tmp/a", b"123").unwrap();
        assert_eq!(global.used(), 3);
        assert!(b.write("/tmp/b", b"456").is_err());
        assert!(b.read("/tmp/b").is_err());
        a.cancel();
        assert_eq!(global.used(), 0);
        b.write("/tmp/b", b"456").unwrap();
        assert_eq!(global.used(), 3);
        b.cancel();
        assert_eq!(global.used(), 0);
    }
    #[test]
    fn worker_control_hook_survives_saturated_host_admission() {
        let hosts = Arc::new(Mutex::new(HostRegistry::default()));
        let config = HostConfig {
            host_budget: Budget::new(1),
            ..Default::default()
        };
        let pool = config.host_budget.clone();
        let context =
            HostContext::new_with_config(Policy::default(), hosts, vec![], config).unwrap();
        context
            .set_worker_handler(Arc::new(|operation, _| {
                Box::pin(async move { Ok(json!(operation)) })
            }))
            .unwrap();
        let _busy = pool.reserve(1).unwrap();
        context.begin_invocation(Duration::from_secs(1)).unwrap();
        assert!(runtime()
            .block_on(context.worker_operation("create".into(), json!({})))
            .is_err());
        assert_eq!(
            runtime()
                .block_on(context.worker_operation("close".into(), json!({})))
                .unwrap(),
            json!("close")
        );
    }
    #[test]
    fn network_default_deny_precedes_resolver_side_effects() {
        struct CountingResolver(Arc<AtomicUsize>);
        impl denosanbox_connectors::Resolver for CountingResolver {
            fn resolve(&self, _: String, _: u16) -> denosanbox_connectors::ResolveFuture {
                self.0.fetch_add(1, Ordering::Relaxed);
                Box::pin(async { Err(ConnectorError::Transport) })
            }
        }
        let count = Arc::new(AtomicUsize::new(0));
        let connector = HttpConnector::with_resolver(
            ConnectorLimits::default(),
            Arc::new(CountingResolver(count.clone())),
        )
        .unwrap();
        let context = HostContext::new(
            Policy::default(),
            Arc::new(Mutex::new(HostRegistry::default())),
            vec![],
        )
        .unwrap();
        context.set_http_connector(connector).unwrap();
        context.begin_invocation(Duration::from_secs(1)).unwrap();
        let result = runtime().block_on(context.http_request(HttpRequest {
            url: "http://localhost/".into(),
            method: "GET".into(),
            headers: Default::default(),
            body: vec![],
        }));
        assert!(matches!(result,Err(Error::Policy{kind,..}) if kind=="network_denied"));
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn resolver_native_work_remains_owned_after_http_deadline() {
        struct SlowResolver {
            pending: Arc<AtomicUsize>,
        }
        impl denosanbox_connectors::Resolver for SlowResolver {
            fn resolve(&self, _: String, _: u16) -> denosanbox_connectors::ResolveFuture {
                let pending = self.pending.clone();
                pending.fetch_add(1, Ordering::AcqRel);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(150));
                    pending.fetch_sub(1, Ordering::AcqRel);
                });
                Box::pin(std::future::pending())
            }
            fn pending(&self) -> usize {
                self.pending.load(Ordering::Acquire)
            }
        }
        let pending = Arc::new(AtomicUsize::new(0));
        let connector = HttpConnector::with_resolver(
            ConnectorLimits::default(),
            Arc::new(SlowResolver {
                pending: pending.clone(),
            }),
        )
        .unwrap();
        let policy = Policy::default().with_network(
            denosanbox_policy::NetworkRules::deny_all()
                .allow_private("localhost", 80)
                .unwrap(),
        );
        let context = HostContext::new(
            policy,
            Arc::new(Mutex::new(HostRegistry::default())),
            vec![],
        )
        .unwrap();
        context.set_http_connector(connector).unwrap();
        context.begin_invocation(Duration::from_millis(30)).unwrap();
        let result = runtime().block_on(context.http_request(HttpRequest {
            url: "http://localhost/".into(),
            method: "GET".into(),
            headers: Default::default(),
            body: vec![],
        }));
        assert!(matches!(result, Err(Error::DeadlineExceeded)));
        assert_eq!(pending.load(Ordering::Acquire), 1);
        assert!(context.outstanding() > 0);
        context.cancel();
        assert!(context.outstanding() > 0);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while context.outstanding() != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(context.outstanding(), 0);
    }
    #[test]
    fn completed_contexts_do_not_accumulate_retired_registry_owners() {
        let registry = Arc::new(Mutex::new(HostRegistry::default()));
        for _ in 0..100 {
            let context = HostContext::new(Policy::default(), registry.clone(), vec![]).unwrap();
            context.cancel();
            assert_eq!(registry.lock().unwrap().retired_owners(), 1);
            drop(context);
            assert_eq!(registry.lock().unwrap().retired_owners(), 0);
        }
    }
}
