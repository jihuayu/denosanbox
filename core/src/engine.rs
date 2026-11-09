use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crate::worker_broker;
use crate::workers::{WorkerHandle, WorkerOptions};
use denosanbox_core::{
    BackendKind, Capabilities, CloseReport, CreatedDriver, Driver, DriverControl, DriverFactory,
    EngineConfig, EngineLimits, Error, FakeFactory, Feature, HostContext, HostRegistry, Invocation,
    InvocationContext, InvocationOptions, PluginRequirements, PluginSource, Policy, PreparedPlugin,
    RegisteredCallback, Result, RuntimeMetrics, SandboxOptions, ShutdownReport, StartOptions,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

const JOIN_GRACE: Duration = Duration::from_secs(5);
const MIN_CALL_TIMEOUT: Duration = Duration::from_millis(1);

/// Engine owning a backend factory and at most `max_active_sandboxes` workers.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    config: EngineConfig,
    factory: Arc<dyn DriverFactory>,
    active: AtomicUsize,
    shutting_down: AtomicBool,
    workers: Mutex<Vec<Weak<WorkerState>>>,
    hosts: Arc<Mutex<HostRegistry>>,
    ceiling: Policy,
    host_config: denosanbox_core::HostConfig,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("backend", &self.backend())
            .field("active_sandboxes", &self.active_sandboxes())
            .finish()
    }
}
impl std::fmt::Debug for SandboxHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxHandle")
            .field(
                "closed",
                &self.state.closed_for_calls.load(Ordering::Acquire),
            )
            .finish()
    }
}
impl Engine {
    /// Creates an engine from validated configuration.
    pub fn new(config: EngineConfig) -> Result<Self> {
        Self::with_policy(config, Policy::default())
    }
    /// Construct with a fixed host authorization ceiling.
    pub fn with_policy(config: EngineConfig, ceiling: Policy) -> Result<Self> {
        config.validate()?;
        let factory: Arc<dyn DriverFactory> = match config.backend {
            BackendKind::Fake => Arc::new(FakeFactory::new()),
            BackendKind::Deno => {
                #[cfg(feature = "deno")]
                {
                    Arc::new(denosanbox_deno::DenoFactory::new())
                }
                #[cfg(not(feature = "deno"))]
                {
                    return Err(Error::RuntimeUnavailable {
                        backend: "deno".into(),
                    });
                }
            }
        };
        Ok(Self {
            inner: Arc::new(EngineInner {
                config,
                factory,
                active: AtomicUsize::new(0),
                shutting_down: AtomicBool::new(false),
                workers: Mutex::new(Vec::new()),
                hosts: Arc::new(Mutex::new(HostRegistry::default())),
                ceiling,
                host_config: denosanbox_core::HostConfig::default(),
            }),
        })
    }

    /// Register a scoped synchronous host callback. It runs outside the isolate thread.
    pub fn register_host(
        &self,
        name: &str,
        input_limit: usize,
        output_limit: usize,
        callback: impl denosanbox_core::policy::Callback + 'static,
    ) -> Result<RegisteredCallback> {
        self.inner
            .hosts
            .lock()
            .unwrap()
            .register(name, input_limit, output_limit, callback)
            .map_err(map_policy)
    }
    /// Register an asynchronous host callback without holding the registry lock.
    pub fn register_host_async<F, Fut>(
        &self,
        name: &str,
        input_limit: usize,
        output_limit: usize,
        callback: F,
    ) -> Result<RegisteredCallback>
    where
        F: Fn(InvocationContext, Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut:
            std::future::Future<Output = denosanbox_core::policy::Result<Vec<u8>>> + Send + 'static,
    {
        self.inner
            .hosts
            .lock()
            .unwrap()
            .register_async(name, input_limit, output_limit, callback)
            .map_err(map_policy)
    }
    /// Backend selected for this engine.
    pub fn backend(&self) -> BackendKind {
        self.inner.factory.kind()
    }

    /// Number of live sandboxes.
    pub fn active_sandboxes(&self) -> usize {
        self.inner.active.load(Ordering::SeqCst)
    }

    /// Configured limits.
    pub fn limits(&self) -> &EngineLimits {
        &self.inner.config.limits
    }

    /// Validates and freezes an immutable source graph.
    pub fn prepare(&self, source: PluginSource) -> Result<PreparedPlugin> {
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        denosanbox_core::prepare(source, &self.inner.config.limits)
    }

    /// Validates and freezes a source graph under an approved compatibility
    /// profile. The profile is preserved on every child entrypoint selection.
    pub fn prepare_with_profile(
        &self,
        source: PluginSource,
        profile: denosanbox_core::modules::CompatibilityProfile,
    ) -> Result<PreparedPlugin> {
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        denosanbox_core::prepare_with_profile(source, &self.inner.config.limits, profile)
    }

    /// Truthful capabilities of this engine's backend. Mediated features are
    /// advertised only because their mediators are wired for every sandbox.
    pub fn capabilities(&self) -> Capabilities {
        let base = self.inner.factory.capabilities();
        if self.inner.config.backend != BackendKind::Deno {
            return base;
        }
        let mut capabilities = base
            .with_feature(Feature::JsonCalls)
            .with_feature(Feature::IndependentIsolates)
            .with_feature(Feature::DeadlineCancellation)
            .with_feature(Feature::VirtualEnvironment)
            .with_feature(Feature::HostCallbacks)
            .with_feature(Feature::VirtualFilesystem)
            .with_feature(Feature::MountedDirectories)
            .with_feature(Feature::Fetch)
            .with_feature(Feature::ManagedChildren)
            .with_feature(Feature::WebWorkers)
            .with_feature(Feature::NodeWorkerThreads);
        // Shared memory and native code are never claimable.
        capabilities = capabilities
            .without_feature(Feature::SharedMemory)
            .without_feature(Feature::NativeCode);
        capabilities
    }

    /// Rejects requirements this engine cannot honor, before any source is
    /// parsed or executed.
    pub fn require_capabilities(&self, requirements: &PluginRequirements) -> Result<()> {
        self.capabilities().require(requirements)
    }

    /// Capability-gated preparation: missing features fail with a typed
    /// [`Error::UnsupportedFeature`] before parsing.
    pub fn prepare_with_requirements(
        &self,
        source: PluginSource,
        requirements: &PluginRequirements,
    ) -> Result<PreparedPlugin> {
        self.require_capabilities(requirements)?;
        self.prepare(source)
    }

    /// Aggregated measured backend metrics for live sandboxes.
    pub async fn metrics(&self) -> RuntimeMetrics {
        let states: Vec<Arc<WorkerState>> = {
            let registry = self.inner.workers.lock().expect("worker registry poisoned");
            registry.iter().filter_map(Weak::upgrade).collect()
        };
        let mut metrics = RuntimeMetrics::default();
        for state in states {
            metrics.merge(&state.measured_metrics().await);
        }
        metrics
    }

    /// Creates a new sandbox with an independent isolate and module instances.
    pub async fn create(
        &self,
        plugin: PreparedPlugin,
        options: SandboxOptions,
    ) -> Result<SandboxHandle> {
        self.create_inner(plugin, options, None).await
    }
    /// Create a registered child using the parent's fixed authorization ceiling.
    pub async fn create_child(
        &self,
        parent: &SandboxHandle,
        plugin: PreparedPlugin,
        mut options: SandboxOptions,
    ) -> Result<SandboxHandle> {
        if parent.state.closed_for_calls.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        if !parent
            .state
            .engine
            .upgrade()
            .is_some_and(|engine| Arc::ptr_eq(&engine, &self.inner))
        {
            return Err(Error::Policy {
                kind: "Denied".into(),
                message: "parent belongs to another Engine".into(),
            });
        }
        if parent.depth() >= 4 {
            return Err(Error::LimitExceeded {
                limit: "worker_depth".into(),
                detail: "maximum Worker depth is four".into(),
            });
        }
        for (name, source) in plugin.iter_modules() {
            if parent.state.prepared.source(name) != Some(source) {
                return Err(Error::ModuleDenied {
                    specifier: name.into(),
                    reason: "child source is outside parent package view".into(),
                });
            }
        }
        options.policy = Policy::child(
            &self.inner.ceiling,
            &parent.state.host.policy(),
            &parent.state.delegation,
            &options.policy,
        )
        .map_err(map_policy)?;
        for grant in &options.host_grants {
            if !parent.state.host_grants.contains(grant) {
                return Err(Error::Policy {
                    kind: "Denied".into(),
                    message: "child callback scope exceeds parent grant".into(),
                });
            }
        }
        self.create_inner(plugin, options, Some(parent)).await
    }
    async fn create_inner(
        &self,
        plugin: PreparedPlugin,
        mut options: SandboxOptions,
        parent: Option<&SandboxHandle>,
    ) -> Result<SandboxHandle> {
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        let default_timeout = options
            .default_invocation_timeout
            .unwrap_or(self.inner.config.limits.default_invocation_timeout);
        // Re-assert this engine's limits on the prepared graph (it may have been
        // prepared by another engine with looser limits) before any thread,
        // isolate, admission or memory reservation is created.
        plugin.validate_with_limits(&self.inner.config.limits)?;
        let default_timeout = default_timeout.min(options.policy.limits().deadline());
        let expires = (Instant::now() + default_timeout).min(
            parent
                .map(|p| p.state.expires)
                .unwrap_or(Instant::now() + default_timeout),
        );
        let default_timeout = expires.saturating_duration_since(Instant::now());
        options.default_invocation_timeout = Some(default_timeout);
        if default_timeout < MIN_CALL_TIMEOUT {
            return Err(Error::InvalidConfiguration {
                reason: "sandbox default timeout must be at least 1ms".to_string(),
            });
        }
        if default_timeout > self.inner.config.limits.max_invocation_timeout {
            return Err(Error::LimitExceeded {
                limit: "max_invocation_timeout".to_string(),
                detail: format!(
                    "sandbox default timeout {:?} exceeds limit {:?}",
                    default_timeout, self.inner.config.limits.max_invocation_timeout
                ),
            });
        }
        if let Some(bytes) = options.memory_limit_bytes {
            if bytes == 0 {
                return Err(Error::InvalidConfiguration {
                    reason: "memory_limit_bytes must be greater than zero".to_string(),
                });
            }
        }

        options.policy = self
            .inner
            .ceiling
            .narrow(&options.policy)
            .map_err(map_policy)?;
        options.memory_limit_bytes = Some(
            options
                .memory_limit_bytes
                .unwrap_or(options.policy.limits().heap_target_bytes())
                .min(options.policy.limits().heap_target_bytes()),
        );
        if self.inner.config.backend == BackendKind::Deno
            && options.memory_limit_bytes.unwrap_or(0) < 8 * 1024 * 1024
        {
            return Err(Error::LimitExceeded {
                limit: "heap_target".into(),
                detail: "Deno requires at least 8MiB including termination reserve".into(),
            });
        }
        let prepared = plugin.clone();
        let subtree_budget = parent
            .map(|p| {
                p.state
                    .subtree_budget
                    .child(options.policy.limits().max_sandboxes() as u64)
            })
            .unwrap_or_else(|| {
                denosanbox_core::policy::Budget::new(options.policy.limits().max_sandboxes() as u64)
            });
        let tree_lease = subtree_budget
            .reserve(1)
            .map_err(|_| Error::LimitExceeded {
                limit: "subtree_sandboxes".into(),
                detail: "ancestor sandbox count exhausted".into(),
            })?;
        let mut guard = ActiveGuard::reserve(self.inner.clone())?;
        guard.tree_lease = Some(tree_lease);
        let host_config = parent
            .map(|p| p.state.host.host_config())
            .unwrap_or_else(|| self.inner.host_config.clone());
        let host = HostContext::new_with_config(
            options.policy.clone(),
            self.inner.hosts.clone(),
            options.host_grants.clone(),
            host_config,
        )?;
        if self.inner.config.backend == BackendKind::Deno {
            guard.heap_lease = Some(
                host.memory_budget()
                    .reserve(options.memory_limit_bytes.unwrap_or(0) as u64)
                    .map_err(|_| Error::LimitExceeded {
                        limit: "subtree_memory_bytes".into(),
                        detail: "ancestor memory reservation exhausted".into(),
                    })?,
            );
        }
        if let Some(parent) = parent {
            host.inherit_mounts_from(&parent.state.host)?;
        }
        host.set_worker_depth(parent.map(|p| p.depth() + 1).unwrap_or(0));
        // Trusted per-context broker and the weak parent factory are bound by
        // Rust before the isolate thread starts. Guests never supply root IDs:
        // the parent's own prepared graph and policy are captured here.
        let base_entrypoint = plugin.entrypoint().to_string();
        host.set_worker_handler(worker_broker::handler(
            self.parent_spawner(&host),
            base_entrypoint,
            self.inner.config.limits.clone(),
        ))?;
        host.begin_invocation(default_timeout)?;
        let CreatedDriver { init, control } =
            self.inner.factory.create(plugin, &options, host.clone())?;

        let capacity = self.inner.config.limits.max_pending_invocations;
        let (sender, receiver) = mpsc::channel(capacity);
        let (start_tx, start_rx) = oneshot::channel();
        let state = Arc::new(WorkerState {
            sender,
            control,
            metrics: Arc::new(Mutex::new(RuntimeMetrics::default())),
            host,
            prepared,
            delegation: options.delegation,
            host_grants: options.host_grants,
            engine: Arc::downgrade(&self.inner),
            children: Mutex::new(Vec::new()),
            depth: parent.map(|p| p.depth() + 1).unwrap_or(0),
            expires,
            subtree_budget,
            close_requested: AtomicBool::new(false),
            closed_for_calls: AtomicBool::new(false),
            in_flight: AtomicBool::new(false),
            completed: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            join: Mutex::new(None),
            report: Mutex::new(None),
            finished: Arc::new(tokio::sync::Notify::new()),
            joined: Arc::new(AtomicBool::new(false)),
            active: Mutex::new(Some(guard)),
            handles: AtomicUsize::new(1),
        });

        if let Some(parent) = parent {
            {
                let mut children = parent.state.children.lock().unwrap();
                if parent.state.closed_for_calls.load(Ordering::Acquire) {
                    return Err(Error::Closed);
                }
                children.retain(|child| child.strong_count() > 0);
                children.push(Arc::downgrade(&state));
            }
        }
        let thread_state = state.clone();
        let join = std::thread::Builder::new()
            .name("denosanbox-worker".to_string())
            .spawn(move || worker_loop(init, receiver, thread_state, start_tx))
            .map_err(|err| Error::Backend {
                message: format!("failed to spawn sandbox worker thread: {err}"),
            })?;
        *state.join.lock().expect("join mutex poisoned") = Some(join);

        {
            let mut workers = self.inner.workers.lock().expect("worker registry poisoned");
            workers.retain(|worker| {
                worker
                    .upgrade()
                    .is_some_and(|state| !state.joined.load(Ordering::Acquire))
            });
            workers.push(Arc::downgrade(&state));
        }
        let mut startup_guard = StartupGuard(Some(state.clone()));

        let startup_deadline = tokio::time::Instant::now() + default_timeout;
        let startup_result = tokio::time::timeout_at(startup_deadline, start_rx).await;
        match startup_result {
            Err(_) => {
                state.request_close();
                let _ = state.close_report().await;
                return Err(Error::DeadlineExceeded);
            }
            Ok(result) => match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    state.request_close();
                    let _ = state.close_report().await;
                    return Err(err);
                }
                Err(_) => {
                    state.request_close();
                    let _ = state.close_report().await;
                    return Err(Error::Backend {
                        message: "sandbox worker exited during startup".to_string(),
                    });
                }
            },
        }
        if self.inner.shutting_down.load(Ordering::SeqCst)
            || state.closed_for_calls.load(Ordering::Acquire)
        {
            state.request_close();
            let _ = state.close_report().await;
            return Err(Error::Closed);
        }
        startup_guard.0.take();
        Ok(SandboxHandle {
            state,
            limits: self.inner.config.limits.clone(),
            default_timeout,
            owns_handle: true,
        })
    }

    /// Builds the trusted parent-side factory handed to the guest broker.
    ///
    /// The closure never accepts a guest-supplied parent identity: the parent
    /// handle (its prepared graph, policy ceiling, delegation, grants and
    /// deadline) is captured by Rust. Children always take the regular
    /// `Engine::create_child` path: delegation intersection, admission,
    /// memory reservation, absolute deadline and parent-close propagation.
    ///
    /// The handler is stored inside this very `HostContext`, so the closure
    /// holds only weak references: owning either the host or the engine here
    /// would form an `Arc` cycle (host -> broker -> host/engine).
    fn parent_spawner(&self, host: &Arc<HostContext>) -> worker_broker::SpawnWorker {
        let host = Arc::downgrade(host);
        let engine = Arc::downgrade(&self.inner);
        Arc::new(move |entrypoint, options| {
            let host = host.clone();
            let engine = engine.clone();
            Box::pin(async move {
                let engine = Engine {
                    inner: engine.upgrade().ok_or(Error::Closed)?,
                };
                engine.spawn_for_guest(&host, &entrypoint, options).await
            })
        })
    }

    /// Resolves the trusted parent handle for `host` from the live registry.
    ///
    /// Only sandboxes registered for guest work are parents, so dropping the
    /// last root handle (cancel/close/abandon) also prevents further spawns.
    fn live_parent(&self, host: &Weak<HostContext>) -> Option<SandboxHandle> {
        let host = host.upgrade()?;
        let registry = self.inner.workers.lock().expect("worker registry poisoned");
        registry
            .iter()
            .filter_map(Weak::upgrade)
            .find(|state| Arc::ptr_eq(&state.host, &host))
            .map(|state| SandboxHandle {
                state,
                limits: self.inner.config.limits.clone(),
                default_timeout: self.inner.config.limits.default_invocation_timeout,
                // A transient view only; it must never cancel on drop.
                owns_handle: false,
            })
    }

    async fn spawn_for_guest(
        &self,
        host: &Weak<HostContext>,
        entrypoint: &str,
        options: SandboxOptions,
    ) -> Result<WorkerHandle> {
        let parent = self.live_parent(host).ok_or(Error::Closed)?;
        engine_child_worker(&parent, entrypoint, options).await
    }

    /// Closes every sandbox owned by this engine.
    ///
    /// Joining happens on native reaper threads, never on the async executor.
    pub async fn shutdown(&self) -> Result<ShutdownReport> {
        self.inner.shutting_down.store(true, Ordering::SeqCst);

        let mut states: Vec<Arc<WorkerState>> = Vec::new();
        {
            let registry = self.inner.workers.lock().expect("worker registry poisoned");
            for weak in registry.iter() {
                if let Some(state) = weak.upgrade() {
                    if !states.iter().any(|known| Arc::ptr_eq(known, &state)) {
                        states.push(state);
                    }
                }
            }
        }

        for state in &states {
            state.request_close();
        }

        let mut report = ShutdownReport::default();
        for state in &states {
            let close = state.close_report().await;
            report.sandboxes_closed += 1;
            if !close.closed_cleanly {
                report.sandboxes_forced += 1;
            }
        }
        report.clean = report.sandboxes_forced == 0;
        Ok(report)
    }
}

/// Handle to one independent sandbox.
pub struct SandboxHandle {
    state: Arc<WorkerState>,
    limits: EngineLimits,
    default_timeout: Duration,
    /// Registered handles count against `WorkerState::handles`; transient
    /// views created by the engine never cancel on drop.
    owns_handle: bool,
}

impl SandboxHandle {
    /// Attach a trusted, nofollow host directory to this sandbox's logical view.
    pub fn mount_directory(
        &self,
        prefix: &str,
        path: &std::path::Path,
        access: denosanbox_core::MountAccess,
    ) -> Result<()> {
        self.state.host.mount_directory(prefix, path, access)
    }
    /// Immutable prepared graph used to create this sandbox.
    pub fn prepared_plugin(&self) -> PreparedPlugin {
        self.state.prepared.clone()
    }
    /// Zero for roots; each registered child increments the depth.
    pub fn depth(&self) -> usize {
        self.state.depth
    }
    /// Host-minted identity. Guest input never controls this value.
    pub fn id(&self) -> denosanbox_core::SandboxId {
        self.state.host.id()
    }
    /// Narrow subsequent policy checks; expansion is rejected.
    pub fn narrow_policy(&self, policy: &Policy) -> Result<()> {
        self.state.host.narrow_policy(policy)
    }
    /// Calls a named ESM export with a JSON input value.
    pub async fn call(
        &self,
        export: &str,
        input: Value,
        options: InvocationOptions,
    ) -> Result<Value> {
        validate_export_name(export)?;
        if self.state.closed_for_calls.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        check_message_size(&input, self.limits.max_message_bytes)?;

        let now = Instant::now();
        let requested_timeout = options
            .timeout
            .or(Some(self.default_timeout))
            .unwrap_or(self.limits.default_invocation_timeout);
        if requested_timeout < MIN_CALL_TIMEOUT {
            return Err(Error::InvalidConfiguration {
                reason: "invocation timeout must be at least 1ms".to_string(),
            });
        }
        if requested_timeout > self.limits.max_invocation_timeout {
            return Err(Error::LimitExceeded {
                limit: "max_invocation_timeout".to_string(),
                detail: format!(
                    "requested timeout {:?} exceeds limit {:?}",
                    requested_timeout, self.limits.max_invocation_timeout
                ),
            });
        }
        let relative_deadline =
            now.checked_add(requested_timeout)
                .ok_or_else(|| Error::InvalidConfiguration {
                    reason: "invocation timeout overflows monotonic clock".into(),
                })?;
        let deadline = match options.deadline {
            Some(absolute) => {
                let absolute_instant = match absolute.duration_since(SystemTime::now()) {
                    Ok(remaining) => Instant::now().checked_add(remaining).ok_or_else(|| {
                        Error::InvalidConfiguration {
                            reason: "invocation deadline overflows monotonic clock".into(),
                        }
                    })?,
                    Err(_) => {
                        return Err(Error::DeadlineExceeded);
                    }
                };
                absolute_instant.min(relative_deadline)
            }
            None => relative_deadline,
        };
        let deadline = deadline.min(self.state.expires);
        if deadline <= Instant::now() {
            return Err(Error::DeadlineExceeded);
        }

        let input_lease = self
            .state
            .host
            .memory_budget()
            .reserve(self.limits.max_message_bytes as u64)
            .map_err(|_| Error::LimitExceeded {
                limit: "queued_message_bytes".into(),
                detail: "ancestor message budget exhausted".into(),
            })?;
        let (reply_tx, reply_rx) = oneshot::channel();
        let invocation = Invocation {
            export: export.to_string(),
            input,
            deadline: Some(SystemTime::now() + deadline.saturating_duration_since(Instant::now())),
            timeout: requested_timeout,
        };
        match self.state.sender.try_send(WorkerMsg::Call(CallJob {
            invocation,
            reply: reply_tx,
            _input_lease: input_lease,
        })) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Err(Error::QueueFull {
                    capacity: self.limits.max_pending_invocations,
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(Error::Closed),
        }

        let tokio_deadline = tokio::time::Instant::from_std(deadline);
        match tokio::time::timeout_at(tokio_deadline, reply_rx).await {
            Ok(Ok(result)) => {
                let value = result?;
                check_message_size(&value, self.limits.max_message_bytes)?;
                Ok(value)
            }
            Ok(Err(_)) => Err(Error::Closed),
            Err(_) => {
                // Deadline is an independent control path, not a queue item.
                self.state.request_close();
                Err(Error::DeadlineExceeded)
            }
        }
    }

    /// Terminates the current invocation. Cancellation is terminal for this
    /// sandbox; calls already queued or later submitted fail with `Closed`.
    pub fn cancel(&self) {
        self.state.request_close();
    }

    /// Requests close and joins the worker without blocking the async executor.
    pub async fn close(&self) -> Result<CloseReport> {
        self.state.request_close();
        Ok(self.state.close_report().await)
    }
}

impl Clone for SandboxHandle {
    fn clone(&self) -> Self {
        if self.owns_handle {
            self.state.handles.fetch_add(1, Ordering::AcqRel);
        }
        Self {
            state: self.state.clone(),
            limits: self.limits.clone(),
            default_timeout: self.default_timeout,
            owns_handle: self.owns_handle,
        }
    }
}

impl Drop for SandboxHandle {
    fn drop(&mut self) {
        // Detach rather than block a runtime from Drop. The worker observes the
        // close flag and exits; an explicit `close()` is required to join/observe.
        if self.owns_handle && self.state.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.state.request_close();
        }
    }
}

struct StartupGuard(Option<Arc<WorkerState>>);
impl Drop for StartupGuard {
    fn drop(&mut self) {
        if let Some(state) = self.0.take() {
            state.request_close();
        }
    }
}

struct WorkerLifetime(Arc<WorkerState>);
impl Drop for WorkerLifetime {
    fn drop(&mut self) {
        let mut report = self.0.report.lock().unwrap_or_else(|err| err.into_inner());
        if report.is_none() {
            *report = Some(CloseReport {
                closed_cleanly: false,
                ..CloseReport::default()
            });
        }
        drop(report);
        self.0.closed_for_calls.store(true, Ordering::Release);
        if let Some(guard) = self
            .0
            .active
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take()
        {
            if self.0.host.outstanding() == 0 {
                drop(guard);
            } else {
                let lease = Arc::new(Mutex::new(Some(guard)));
                let retained = lease.clone();
                let host = self.0.host.clone();
                if std::thread::Builder::new()
                    .name("denosanbox-accounting".into())
                    .spawn(move || {
                        while host.outstanding() != 0 {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        retained.lock().unwrap().take();
                    })
                    .is_err()
                {
                    std::mem::forget(lease);
                }
            }
        }
        self.0.finished.notify_waiters();
    }
}

struct ActiveGuard {
    engine: Arc<EngineInner>,
    tree_lease: Option<denosanbox_core::policy::Lease>,
    heap_lease: Option<denosanbox_core::policy::Lease>,
}

impl ActiveGuard {
    fn reserve(engine: Arc<EngineInner>) -> Result<Self> {
        let max = engine.config.limits.max_active_sandboxes;
        loop {
            let current = engine.active.load(Ordering::SeqCst);
            if current >= max {
                return Err(Error::LimitExceeded {
                    limit: "max_active_sandboxes".to_string(),
                    detail: format!("{current} sandboxes are already active (limit {max})"),
                });
            }
            if engine
                .active
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(Self {
                    engine,
                    tree_lease: None,
                    heap_lease: None,
                });
            }
        }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.engine.active.fetch_sub(1, Ordering::SeqCst);
    }
}

enum WorkerMsg {
    Call(CallJob),
    Close,
}

struct CallJob {
    invocation: Invocation,
    reply: oneshot::Sender<Result<Value>>,
    _input_lease: denosanbox_core::policy::Lease,
}

struct WorkerState {
    sender: mpsc::Sender<WorkerMsg>,
    control: Arc<dyn DriverControl>,
    /// Measured backend snapshots; refreshed by the worker thread after each
    /// invocation so reporting never contends with guest execution.
    metrics: Arc<Mutex<RuntimeMetrics>>,
    host: Arc<HostContext>,
    prepared: PreparedPlugin,
    delegation: Policy,
    host_grants: Vec<denosanbox_core::HostGrant>,
    engine: Weak<EngineInner>,
    children: Mutex<Vec<Weak<WorkerState>>>,
    depth: usize,
    expires: Instant,
    subtree_budget: Arc<denosanbox_core::policy::Budget>,
    close_requested: AtomicBool,
    closed_for_calls: AtomicBool,
    in_flight: AtomicBool,
    completed: AtomicU64,
    cancelled: AtomicU64,
    join: Mutex<Option<JoinHandle<()>>>,
    report: Mutex<Option<CloseReport>>,
    finished: Arc<tokio::sync::Notify>,
    joined: Arc<AtomicBool>,
    active: Mutex<Option<ActiveGuard>>,
    handles: AtomicUsize,
}

impl WorkerState {
    /// Best-effort measured metrics. Never blocks on the worker thread long:
    /// the cached snapshot is used if the live query is not answered quickly.
    async fn measured_metrics(&self) -> RuntimeMetrics {
        self.metrics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn request_close(&self) {
        if !self.close_requested.swap(true, Ordering::SeqCst) {
            self.closed_for_calls.store(true, Ordering::SeqCst);
            self.control.cancel();
            self.host.cancel();
            for child in self
                .children
                .lock()
                .unwrap()
                .iter()
                .filter_map(Weak::upgrade)
            {
                child.request_close();
            }
            let _ = self.sender.try_send(WorkerMsg::Close);
        }
    }

    fn close_report(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CloseReport> + Send + '_>> {
        Box::pin(async move {
            let handle = self.join.lock().expect("join mutex poisoned").take();
            if let Some(handle) = handle {
                let joined = self.joined.clone();
                let finished = self.finished.clone();
                // Native reaping cannot pin the async host runtime's blocking pool.
                let _ = std::thread::Builder::new()
                    .name("denosanbox-reaper".into())
                    .spawn(move || {
                        let _ = handle.join();
                        joined.store(true, Ordering::Release);
                        finished.notify_waiters();
                    });
            }
            let expires = tokio::time::Instant::now() + JOIN_GRACE;
            loop {
                let notified = self.finished.notified();
                if self.joined.load(Ordering::Acquire) {
                    break;
                }
                if tokio::time::timeout_at(expires, notified).await.is_err() {
                    break;
                }
            }
            let joined = self.joined.load(Ordering::Acquire);
            let mut report = self
                .report
                .lock()
                .expect("report mutex poisoned")
                .clone()
                .unwrap_or_else(|| CloseReport {
                    invocations_completed: self.completed.load(Ordering::SeqCst),
                    invocations_cancelled: self.cancelled.load(Ordering::SeqCst),
                    forced_termination: self.control.is_cancelled(),
                    closed_cleanly: false,
                    pending_host_operations: self.host.outstanding(),
                });
            if !joined {
                report.closed_cleanly = false;
                report.forced_termination = true;
            }
            report.pending_host_operations = self.host.outstanding();
            let children: Vec<_> = self
                .children
                .lock()
                .unwrap()
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
            for child in children {
                child.request_close();
                let closed = child.close_report().await;
                report.closed_cleanly &= closed.closed_cleanly;
                report.pending_host_operations += closed.pending_host_operations;
            }
            report.closed_cleanly &= report.pending_host_operations == 0;
            report
        })
    }
}

fn worker_loop(
    init: Box<dyn FnOnce() -> Result<Box<dyn Driver>> + Send>,
    mut receiver: mpsc::Receiver<WorkerMsg>,
    state: Arc<WorkerState>,
    startup: oneshot::Sender<Result<()>>,
) {
    let _lifetime = WorkerLifetime(state.clone());
    let mut driver = match init() {
        Ok(driver) => driver,
        Err(err) => {
            let _ = startup.send(Err(err));
            *state.report.lock().expect("report mutex poisoned") = Some(CloseReport {
                closed_cleanly: true,
                ..CloseReport::default()
            });

            state.finished.notify_waiters();
            return;
        }
    };
    let state_for_metrics = state.clone();
    let start_options = StartOptions {
        on_invocation: Some(Arc::new(move |update: &RuntimeMetrics| {
            state_for_metrics
                .metrics
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .merge(update);
        })),
        // The Deno backend acknowledges its own spawn id on the owner thread
        // before the first guest message; other backends have no handshake.
        ack: None,
    };
    record_metric(&state.metrics, &driver.metrics());
    let started = driver.start_with(start_options);
    let start_ok = started.is_ok();
    let _ = startup.send(started);
    if !start_ok {
        let report = driver.close().unwrap_or(CloseReport {
            closed_cleanly: false,
            ..CloseReport::default()
        });
        *state.report.lock().expect("report mutex poisoned") = Some(report);

        state.finished.notify_waiters();
        return;
    }

    loop {
        if state.close_requested.load(Ordering::SeqCst) {
            break;
        }
        let msg = match receiver.blocking_recv() {
            Some(msg) => msg,
            None => break,
        };
        match msg {
            WorkerMsg::Close => break,
            WorkerMsg::Call(job) => {
                if state.close_requested.load(Ordering::SeqCst) {
                    state.cancelled.fetch_add(1, Ordering::SeqCst);
                    let _ = job.reply.send(Err(Error::Closed));
                    continue;
                }
                state.in_flight.store(true, Ordering::SeqCst);
                if state.close_requested.load(Ordering::SeqCst) {
                    state.control.cancel();
                }
                let duration = job
                    .invocation
                    .deadline
                    .and_then(|d| d.duration_since(SystemTime::now()).ok())
                    .unwrap_or(Duration::ZERO);
                let result = state
                    .host
                    .begin_invocation(duration)
                    .and_then(|_| driver.invoke(job.invocation));
                state.in_flight.store(false, Ordering::SeqCst);
                match &result {
                    Ok(_) => {
                        state.completed.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(Error::Cancelled) => {
                        state.cancelled.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(_) => {}
                }
                let _ = job.reply.send(result);
                record_metric(&state.metrics, &driver.metrics());
                if state.close_requested.load(Ordering::SeqCst) {
                    break;
                }
            }
        }
    }

    state.closed_for_calls.store(true, Ordering::SeqCst);
    while let Ok(msg) = receiver.try_recv() {
        if let WorkerMsg::Call(job) = msg {
            state.cancelled.fetch_add(1, Ordering::SeqCst);
            let _ = job.reply.send(Err(Error::Closed));
        }
    }
    let report = driver.close().unwrap_or(CloseReport {
        invocations_completed: state.completed.load(Ordering::SeqCst),
        invocations_cancelled: state.cancelled.load(Ordering::SeqCst),
        forced_termination: state.control.is_cancelled(),
        closed_cleanly: false,
        pending_host_operations: state.host.outstanding(),
    });
    *state.report.lock().expect("report mutex poisoned") = Some(report);

    state.finished.notify_waiters();
}

/// Spawn outside any engine lock. The caller has already resolved `parent`
/// from the live registry (a transient, non-owning handle).
async fn engine_child_worker(
    parent: &SandboxHandle,
    entrypoint: &str,
    options: SandboxOptions,
) -> Result<WorkerHandle> {
    let engine_inner = parent.state.engine.upgrade().ok_or(Error::Closed)?;
    let plugin = parent.prepared_plugin().resolve_child_entrypoint(entrypoint)?;
    let engine = Engine {
        inner: engine_inner.clone(),
    };
    let child = engine.create_child(parent, plugin, options).await?;
    Ok(WorkerHandle::from_sandbox(
        child,
        WorkerOptions {
            queue_capacity: worker_broker::MAX_QUEUED_MESSAGES,
            max_queued_bytes: worker_broker::MAX_QUEUED_BYTES,
            max_message_bytes: engine_inner.config.limits.max_message_bytes,
            invocation_timeout: worker_broker::MESSAGE_TIMEOUT,
        },
    ))
}

fn record_metric(cell: &Mutex<RuntimeMetrics>, update: &RuntimeMetrics) {
    cell.lock().unwrap_or_else(|e| e.into_inner()).merge(update);
}

fn map_policy(error: denosanbox_core::policy::Error) -> Error {
    Error::Policy {
        kind: format!("{:?}", error.kind()),
        message: format!("sandbox policy: {:?}", error.kind()),
    }
}

fn validate_export_name(export: &str) -> Result<()> {
    if export.is_empty() || export.len() > 128 {
        return Err(Error::InvalidConfiguration {
            reason: "export name must be between 1 and 128 bytes".to_string(),
        });
    }
    let mut chars = export.chars();
    let first = chars.next().expect("checked non-empty");
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return Err(Error::InvalidConfiguration {
            reason: format!("export name '{export}' is not a JavaScript identifier"),
        });
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
        return Err(Error::InvalidConfiguration {
            reason: format!("export name '{export}' is not a JavaScript identifier"),
        });
    }
    Ok(())
}

fn check_message_size(value: &Value, max: usize) -> Result<()> {
    struct Limited {
        written: usize,
        max: usize,
    }
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.written = self.written.saturating_add(bytes.len());
            if self.written > self.max {
                return Err(std::io::Error::other("message limit exceeded"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Limited { written: 0, max };
    serde_json::to_writer(&mut counter, value).map_err(|_| Error::LimitExceeded {
        limit: "max_message_bytes".into(),
        detail: "serialized message exceeds byte limit".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fake_engine() -> Engine {
        Engine::new(EngineConfig::new(BackendKind::Fake)).unwrap()
    }

    fn plugin() -> PreparedPlugin {
        fake_engine()
            .prepare(PluginSource::single(
                "main.js",
                "export async function run(input) { return input; }",
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn queue_saturation_is_typed_and_bounded() {
        let engine = fake_engine();
        let sandbox = engine
            .create(plugin(), SandboxOptions::default())
            .await
            .unwrap();
        let blocker = sandbox.clone();
        let first = tokio::spawn(async move {
            blocker
                .call("block", json!({}), InvocationOptions::default())
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut queued = Vec::new();
        for _ in 0..engine.limits().max_pending_invocations {
            let handle = sandbox.clone();
            queued.push(tokio::spawn(async move {
                handle
                    .call("echo", json!(1), InvocationOptions::default())
                    .await
            }));
        }
        while sandbox.state.sender.capacity() > 0 {
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            sandbox
                .call("echo", json!(1), InvocationOptions::default())
                .await,
            Err(Error::QueueFull { .. })
        ));

        sandbox.cancel();
        let _ = first.await.unwrap();
        for queued in queued {
            let _ = queued.await.unwrap();
        }
        let _ = sandbox.close().await.unwrap();
    }

    #[tokio::test]
    async fn sandboxes_have_independent_state() {
        let engine = fake_engine();
        let a = engine
            .create(plugin(), SandboxOptions::default())
            .await
            .unwrap();
        let b = engine
            .create(plugin(), SandboxOptions::default())
            .await
            .unwrap();
        a.call(
            "set",
            json!({"key": "k", "value": "only-a"}),
            InvocationOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            b.call("get", json!({"key": "k"}), InvocationOptions::default())
                .await
                .unwrap(),
            Value::Null
        );
        assert_eq!(
            a.call("get", json!({"key": "k"}), InvocationOptions::default())
                .await
                .unwrap(),
            json!("only-a")
        );
        a.close().await.unwrap();
        b.close().await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_does_not_affect_b() {
        let engine = fake_engine();
        let a = engine
            .create(plugin(), SandboxOptions::default())
            .await
            .unwrap();
        let b = engine
            .create(plugin(), SandboxOptions::default())
            .await
            .unwrap();
        let a_clone = a.clone();
        let blocked = tokio::spawn(async move {
            a_clone
                .call("block", json!({}), InvocationOptions::default())
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        a.cancel();
        let result = blocked.await.unwrap();
        assert!(matches!(result, Err(Error::Cancelled | Error::Closed)));
        let after = a.call("echo", json!(1), InvocationOptions::default()).await;
        assert!(matches!(after, Err(Error::Closed)));

        assert_eq!(
            b.call(
                "echo",
                json!({"still": "alive"}),
                InvocationOptions::default()
            )
            .await
            .unwrap(),
            json!({"still": "alive"})
        );
        assert!(a.close().await.unwrap().closed_cleanly);
        assert!(b.close().await.unwrap().closed_cleanly);
    }

    #[tokio::test]
    async fn absolute_deadline_terminates_runaway_call() {
        let engine = fake_engine();
        let sandbox = engine
            .create(plugin(), SandboxOptions::default())
            .await
            .unwrap();
        let result = sandbox
            .call(
                "block",
                json!({}),
                InvocationOptions {
                    timeout: None,
                    deadline: Some(SystemTime::now() + Duration::from_millis(40)),
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(Error::DeadlineExceeded | Error::Cancelled)
        ));
        sandbox.close().await.unwrap();
    }

    #[tokio::test]
    async fn engine_isolation_and_shutdown() {
        let engine = fake_engine();
        let prepared = plugin();
        let a = engine
            .create(prepared.clone(), SandboxOptions::default())
            .await
            .unwrap();
        let b = engine
            .create(prepared, SandboxOptions::default())
            .await
            .unwrap();
        assert_eq!(engine.active_sandboxes(), 2);
        assert_eq!(engine.backend(), BackendKind::Fake);
        let report = engine.shutdown().await.unwrap();
        assert_eq!(report.sandboxes_closed, 2);
        assert!(report.clean);
        assert!(matches!(
            a.call("echo", json!(1), InvocationOptions::default()).await,
            Err(Error::Closed)
        ));
        assert!(matches!(
            b.call("echo", json!(1), InvocationOptions::default()).await,
            Err(Error::Closed)
        ));
    }

    #[tokio::test]
    async fn active_sandbox_limit_is_enforced() {
        let mut config = EngineConfig::new(BackendKind::Fake);
        config.limits.max_active_sandboxes = 2;
        let engine = Engine::new(config).unwrap();
        let prepared = plugin();
        let a = engine
            .create(prepared.clone(), SandboxOptions::default())
            .await
            .unwrap();
        let b = engine
            .create(prepared.clone(), SandboxOptions::default())
            .await
            .unwrap();
        let third = engine
            .create(prepared, SandboxOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(third, Error::LimitExceeded { .. }));
        a.close().await.unwrap();
        b.close().await.unwrap();
    }

    #[test]
    fn unsupported_backend_is_typed_when_not_compiled() {
        // This assertion is meaningful only without the default `deno` feature.
        #[cfg(not(feature = "deno"))]
        {
            let err = Engine::new(EngineConfig::new(BackendKind::Deno)).unwrap_err();
            assert!(matches!(err, Error::RuntimeUnavailable { .. }));
            assert_eq!(err.kind(), "runtime_unavailable");
        }
    }

    #[tokio::test]
    async fn message_bound_is_typed_both_ways() {
        let mut config = EngineConfig::new(BackendKind::Fake);
        config.limits.max_message_bytes = 64;
        let engine = Engine::new(config).unwrap();
        let sandbox = engine
            .create(plugin(), SandboxOptions::default())
            .await
            .unwrap();
        let big = "x".repeat(200);
        let err = sandbox
            .call("echo", json!({"x": big}), InvocationOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::LimitExceeded { .. }));
        sandbox.close().await.unwrap();
    }
}
