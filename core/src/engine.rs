use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use denosanbox_core::{
    BackendKind, CloseReport, CreatedDriver, Driver, DriverControl, DriverFactory, EngineConfig,
    EngineLimits, Error, FakeFactory, Invocation, InvocationOptions, PluginSource, PreparedPlugin,
    Result, SandboxOptions, ShutdownReport,
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
            }),
        })
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

    /// Creates a new sandbox with an independent isolate and module instances.
    pub async fn create(
        &self,
        plugin: PreparedPlugin,
        options: SandboxOptions,
    ) -> Result<SandboxHandle> {
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        let default_timeout = options
            .default_invocation_timeout
            .unwrap_or(self.inner.config.limits.default_invocation_timeout);
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

        let guard = ActiveGuard::reserve(self.inner.clone())?;
        let CreatedDriver { init, control } = self.inner.factory.create(plugin, &options)?;

        let capacity = self.inner.config.limits.max_pending_invocations;
        let (sender, receiver) = mpsc::channel(capacity);
        let (start_tx, start_rx) = oneshot::channel();
        let state = Arc::new(WorkerState {
            sender,
            control,
            close_requested: AtomicBool::new(false),
            closed_for_calls: AtomicBool::new(false),
            in_flight: AtomicBool::new(false),
            completed: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            join: Mutex::new(None),
            report: Mutex::new(None),
            finished: tokio::sync::Notify::new(),
            active: Mutex::new(Some(guard)),
            handles: AtomicUsize::new(1),
        });

        let thread_state = state.clone();
        let join = std::thread::Builder::new()
            .name("denosanbox-worker".to_string())
            .spawn(move || worker_loop(init, receiver, thread_state, start_tx))
            .map_err(|err| Error::Backend {
                message: format!("failed to spawn sandbox worker thread: {err}"),
            })?;
        *state.join.lock().expect("join mutex poisoned") = Some(join);

        self.inner
            .workers
            .lock()
            .expect("worker registry poisoned")
            .push(Arc::downgrade(&state));

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
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            state.request_close();
            let _ = state.close_report().await;
            return Err(Error::Closed);
        }
        Ok(SandboxHandle {
            state,
            limits: self.inner.config.limits.clone(),
            default_timeout,
        })
    }

    /// Closes every sandbox owned by this engine.
    ///
    /// Joining happens in `spawn_blocking`, never on the async executor.
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
}

impl SandboxHandle {
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
        let relative_deadline = now + requested_timeout;
        let deadline = match options.deadline {
            Some(absolute) => {
                let absolute_instant = match absolute.duration_since(SystemTime::now()) {
                    Ok(remaining) => Instant::now() + remaining,
                    Err(_) => {
                        return Err(Error::DeadlineExceeded);
                    }
                };
                absolute_instant.min(relative_deadline)
            }
            None => relative_deadline,
        };
        if deadline <= Instant::now() {
            return Err(Error::DeadlineExceeded);
        }

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
        self.state.handles.fetch_add(1, Ordering::AcqRel);
        Self {
            state: self.state.clone(),
            limits: self.limits.clone(),
            default_timeout: self.default_timeout,
        }
    }
}

impl Drop for SandboxHandle {
    fn drop(&mut self) {
        // Detach rather than block a runtime from Drop. The worker observes the
        // close flag and exits; an explicit `close()` is required to join/observe.
        if self.state.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.state.request_close();
        }
    }
}

struct ActiveGuard {
    engine: Arc<EngineInner>,
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
                return Ok(Self { engine });
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
}

struct WorkerState {
    sender: mpsc::Sender<WorkerMsg>,
    control: Arc<dyn DriverControl>,
    close_requested: AtomicBool,
    closed_for_calls: AtomicBool,
    in_flight: AtomicBool,
    completed: AtomicU64,
    cancelled: AtomicU64,
    join: Mutex<Option<JoinHandle<()>>>,
    report: Mutex<Option<CloseReport>>,
    finished: tokio::sync::Notify,
    active: Mutex<Option<ActiveGuard>>,
    handles: AtomicUsize,
}

impl WorkerState {
    fn request_close(&self) {
        if !self.close_requested.swap(true, Ordering::SeqCst) {
            self.closed_for_calls.store(true, Ordering::SeqCst);
            self.control.cancel();
            let _ = self.sender.try_send(WorkerMsg::Close);
        }
    }

    async fn close_report(&self) -> CloseReport {
        let handle = self.join.lock().expect("join mutex poisoned").take();
        let mut joined = true;
        if let Some(handle) = handle {
            let join = tokio::task::spawn_blocking(move || {
                let _ = handle.join();
            });
            match tokio::time::timeout(JOIN_GRACE, join).await {
                Ok(_) => {}
                Err(_) => joined = false,
            }
        }
        if joined {
            // The worker publishes the report before exiting. Wait briefly if the
            // join completed through another concurrent close call.
            for _ in 0..50 {
                if self.report.lock().expect("report mutex poisoned").is_some() {
                    break;
                }
                let _ =
                    tokio::time::timeout(Duration::from_millis(2), self.finished.notified()).await;
            }
        }
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
            });
        if !joined {
            report.closed_cleanly = false;
            report.forced_termination = true;
        }
        report
    }
}

fn worker_loop(
    init: Box<dyn FnOnce() -> Result<Box<dyn Driver>> + Send>,
    mut receiver: mpsc::Receiver<WorkerMsg>,
    state: Arc<WorkerState>,
    startup: oneshot::Sender<Result<()>>,
) {
    let mut driver = match init() {
        Ok(driver) => driver,
        Err(err) => {
            let _ = startup.send(Err(err));
            *state.report.lock().expect("report mutex poisoned") = Some(CloseReport {
                closed_cleanly: true,
                ..CloseReport::default()
            });
            state.active.lock().unwrap().take();
            state.finished.notify_waiters();
            return;
        }
    };
    let started = driver.start();
    let start_ok = started.is_ok();
    let _ = startup.send(started);
    if !start_ok {
        let report = driver.close().unwrap_or(CloseReport {
            closed_cleanly: false,
            ..CloseReport::default()
        });
        *state.report.lock().expect("report mutex poisoned") = Some(report);
        state.active.lock().unwrap().take();
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
                let result = driver.invoke(job.invocation);
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
    });
    *state.report.lock().expect("report mutex poisoned") = Some(report);
    state.active.lock().unwrap().take();
    state.finished.notify_waiters();
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
    let size = serde_json::to_vec(value)
        .map_err(|err| Error::Internal {
            message: format!("failed to serialize message: {err}"),
        })?
        .len();
    if size > max {
        return Err(Error::LimitExceeded {
            limit: "max_message_bytes".to_string(),
            detail: format!("message is {size} bytes but the limit is {max}"),
        });
    }
    Ok(())
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
