//! Thread-affine in-process deno_core/V8 backend. No guest operating-system ops.
mod buffers;
use deno_core::error::ModuleLoaderError;
use deno_core::OpState;
use deno_core::{
    v8, JsRuntime, ModuleId, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse,
    ModuleLoader, ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType,
    PollEventLoopOptions, ResolutionKind, RuntimeOptions,
};
use denosanbox_core::modules::CompatibilityProfile;
use denosanbox_core::{builtin_alias_target, is_reserved_builtin_path};
use denosanbox_core::{
    BackendKind, CloseReport, CreatedDriver, Driver, DriverControl, DriverFactory, Error,
    Invocation, PreparedPlugin, Result, RuntimeMetrics, SandboxOptions, StartOptions,
};

/// Guest-visible message/encode cap used by the frozen bootstrap closures.
const BOOTSTRAP_MESSAGE_LIMIT: usize = 1024 * 1024;
/// Hard bound on one serialized log record handed to the host sink.
const MAX_LOG_RECORD_BYTES: usize = 64 * 1024;
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, SystemTime};

/// Parent-inbox limits mirrored into the frozen bootstrap closures.
mod worker_broker_limits {
    pub const QUEUE: usize = 32;
    pub const BYTES: usize = 4 * 1024 * 1024;
}

/// Trusted bootstrap installed before the raw `Deno` namespace is removed.
const BOOTSTRAP: &str = include_str!("worker_bootstrap.js");

struct Control {
    host: Arc<denosanbox_core::HostContext>,
    running: AtomicBool,
    interrupted: AtomicBool,
    heap_exceeded: AtomicBool,
    log_bytes: std::sync::atomic::AtomicUsize,
    cancelled: AtomicBool,
    timed_out: AtomicBool,
    isolate: Mutex<Option<v8::IsolateHandle>>,
    metrics: Arc<MetricsShared>,
    heap_limit: Arc<std::sync::atomic::AtomicUsize>,
}
impl Control {
    fn terminate(&self) {
        if let Some(isolate) = self.isolate.lock().unwrap().as_ref() {
            isolate.terminate_execution();
        }
    }
    fn error(&self, fallback: impl ToString) -> Error {
        if self.heap_exceeded.load(Ordering::Acquire) {
            Error::LimitExceeded {
                limit: "js_heap_target".into(),
                detail: "JavaScript heap target exceeded".into(),
            }
        } else if self.timed_out.load(Ordering::Acquire) {
            Error::DeadlineExceeded
        } else if self.cancelled.load(Ordering::Acquire) {
            Error::Cancelled
        } else {
            Error::Execution {
                message: fallback.to_string(),
            }
        }
    }
}
impl DriverControl for Control {
    fn cancel(&self) {
        if self.running.load(Ordering::Acquire) {
            self.interrupted.store(true, Ordering::Release);
        }
        self.cancelled.store(true, Ordering::Release);
        self.host.cancel();
        self.terminate();
    }
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.timed_out.load(Ordering::Acquire)
    }
    fn metrics(&self) -> RuntimeMetrics {
        let limit = self.heap_limit.load(Ordering::Acquire);
        self.metrics.snapshot((limit != 0).then_some(limit))
    }
}

/// Counters written only by the owning worker thread, read through the
/// `DriverControl` handle used for reporting.
#[derive(Default)]
struct MetricsShared {
    completed: std::sync::atomic::AtomicU64,
    cancelled: std::sync::atomic::AtomicU64,
    heap: Mutex<Option<(usize, usize)>>,
}

impl MetricsShared {
    fn set_heap(&self, used: usize, total: usize) {
        *self.heap.lock().unwrap_or_else(|e| e.into_inner()) = Some((used, total));
    }

    /// Exact snapshot. `heap_limit` is the configured V8 heap bound, if any.
    fn snapshot(&self, heap_limit: Option<usize>) -> RuntimeMetrics {
        let heap = *self.heap.lock().unwrap_or_else(|e| e.into_inner());
        RuntimeMetrics {
            completed_invocations: Some(self.completed.load(Ordering::Acquire)),
            cancelled_invocations: Some(self.cancelled.load(Ordering::Acquire)),
            heap_used_bytes: heap.map(|(used, _)| used),
            heap_limit_bytes: heap_limit,
        }
    }
}
struct Watchdog {
    control: Arc<Control>,
    done: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Watchdog {
    fn new(control: Arc<Control>, timeout: Duration) -> Result<Self> {
        control.running.store(true, Ordering::Release);
        control.log_bytes.store(0, Ordering::Release);
        let watch_control = control.clone();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("denosanbox-watchdog".into())
            .spawn(move || {
                if matches!(
                    rx.recv_timeout(timeout),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    control.timed_out.store(true, Ordering::Release);
                    control.host.cancel();
                    control.terminate();
                }
            })
            .map_err(|e| Error::Backend {
                message: e.to_string(),
            })?;
        Ok(Self {
            control: watch_control,
            done: Some(tx),
            thread: Some(thread),
        })
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        self.done.take();
        self.control.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
pub struct DenoFactory;
impl DenoFactory {
    pub fn new() -> Self {
        Self
    }
}
impl DriverFactory for DenoFactory {
    fn kind(&self) -> BackendKind {
        BackendKind::Deno
    }
    fn create(
        &self,
        plugin: PreparedPlugin,
        options: &SandboxOptions,
        context: Arc<denosanbox_core::HostContext>,
    ) -> Result<CreatedDriver> {
        let metrics = Arc::new(MetricsShared::default());
        let heap_limit = Arc::new(std::sync::atomic::AtomicUsize::new(
            options
                .memory_limit_bytes
                .map(|bytes| bytes.saturating_sub(4 * 1024 * 1024))
                .unwrap_or(0),
        ));
        let driver_heap_limit = {
            let limit = heap_limit.load(Ordering::Acquire);
            (limit != 0).then_some(limit)
        };
        let control = Arc::new(Control {
            host: context.clone(),
            running: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            heap_exceeded: AtomicBool::new(false),
            log_bytes: std::sync::atomic::AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            isolate: Mutex::new(None),
            metrics: metrics.clone(),
            heap_limit,
        });
        let thread_control = control.clone();
        let options = options.clone();
        Ok(CreatedDriver {
            control,
            init: Box::new(move || {
                let executor = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .map_err(|e| Error::Backend {
                        message: e.to_string(),
                    })?;
                // deno_core registers the isolate's tokio handle only while a
                // runtime context is entered. V8 may post a *delayed* task from
                // JsRuntime::new itself (GC memory reducer); without the captured
                // handle deno_core calls `process::abort()`. Hold the guard for
                // all of V8/isolate creation and the bootstrap, and never store
                // it inside `DenoDriver` (a Runtime cannot be dropped inside its
                // own context): the driver owns `executor` and drops it from
                // outside any guard.
                let enter_guard = executor.enter();
                let mut runtime = JsRuntime::new(RuntimeOptions {
                    module_loader: Some(Rc::new(GraphLoader::new(plugin.clone()))),
                    extensions: vec![sandbox_ops::init(context.clone(), thread_control.clone())],
                    create_params: options.memory_limit_bytes.map(|bytes| {
                        v8::CreateParams::default()
                            .heap_limits(0, bytes.saturating_sub(4 * 1024 * 1024))
                            .array_buffer_allocator(buffers::allocator(context.memory_budget()))
                    }),
                    ..Default::default()
                });
                let heap_control = thread_control.clone();
                runtime.add_near_heap_limit_callback(move |current, initial| {
                    heap_control.heap_exceeded.store(true, Ordering::Release);
                    heap_control.host.cancel();
                    heap_control.terminate();
                    current.max(initial.saturating_add(4 * 1024 * 1024))
                });
                *thread_control.isolate.lock().unwrap() =
                    Some(runtime.v8_isolate().thread_safe_handle());
                if thread_control.is_cancelled() {
                    thread_control.terminate();
                    return Err(Error::Cancelled);
                }
                // One self-invoking bootstrap: it captures the trusted ops,
                // hides the raw namespace, installs mediated globals and
                // asserts the raw namespace is unreachable. The profile flag
                // comes from the trusted prepared graph, never from guest input.
                let node_worker_threads =
                    plugin.profile() == CompatibilityProfile::PureJsNpm
                        && plugin.has_reserved_builtin(denosanbox_core::WORKER_THREADS_BUILTIN);
                let config = format!(
                    "{{nodeWorkerThreads:{node_worker_threads},maxMessageBytes:{limit},maxLogRecordBytes:{record},maxQueuedMessages:{queued},maxQueuedBytes:{bytes},maxChildren:{children},isChild:{child}}}",
                    limit = BOOTSTRAP_MESSAGE_LIMIT,
                    record = MAX_LOG_RECORD_BYTES,
                    queued = worker_broker_limits::QUEUE,
                    bytes = worker_broker_limits::BYTES,
                    children = context.policy().limits().max_sandboxes(),
                    child = context.worker_depth() > 0,
                );
                let bootstrap = BOOTSTRAP.replace("__SANDBOX_CONFIG__", &config);
                runtime
                    .execute_script("sandbox-bootstrap.js", bootstrap)
                    .map_err(|e| thread_control.error(e))?;
                drop(enter_guard);
                Ok(Box::new(DenoDriver {
                    runtime,
                    executor,
                    control: thread_control,
                    plugin,
                    module: None,
                    timeout: options
                        .default_invocation_timeout
                        .unwrap_or(Duration::from_secs(30)),
                    completed: 0,
                    cancelled: 0,
                    metrics: metrics.clone(),
                    observer: None,
                    heap_limit: driver_heap_limit,
                }) as Box<dyn Driver>)
            }),
        })
    }
}
/// Module loader bound to one immutable prepared graph.
///
/// Only `sandbox:///` paths present in the graph load. The Node
/// `worker_threads` aliases resolve to the trusted reserved builtin and only
/// when that profile actually injected it; every other bare, `node:` or URL
/// specifier is denied, including computed dynamic imports.
struct GraphLoader {
    plugin: PreparedPlugin,
}

impl GraphLoader {
    fn new(plugin: PreparedPlugin) -> Self {
        Self { plugin }
    }

    fn checked(&self, url: &ModuleSpecifier) -> std::result::Result<String, ModuleLoaderError> {
        if url.scheme() != "sandbox"
            || url.host_str().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ModuleLoaderError::generic(
                "module scheme or authority denied",
            ));
        }
        let path = url.path().trim_start_matches('/');
        denosanbox_core::validate_path(path)
            .map_err(|e| ModuleLoaderError::generic(e.to_string()))?;
        if self.plugin.source(path).is_none() {
            return Err(ModuleLoaderError::generic(
                "module is outside approved graph",
            ));
        }
        Ok(path.to_owned())
    }

    /// Trusted reserved module visible to guests only via an alias.
    fn alias_allowed(&self, path: &str) -> bool {
        is_reserved_builtin_path(path)
            && self.plugin.profile() == CompatibilityProfile::PureJsNpm
            && self.plugin.has_reserved_builtin(path)
    }

    fn alias_module(
        &self,
        specifier: &str,
    ) -> std::result::Result<ModuleSpecifier, ModuleLoaderError> {
        let target = builtin_alias_target(specifier).ok_or_else(|| {
            ModuleLoaderError::generic("only relative graph imports are permitted")
        })?;
        if !self.alias_allowed(target) {
            return Err(ModuleLoaderError::generic(format!(
                "'{specifier}' requires the approved profile and an injected trusted builtin"
            )));
        }
        let url = ModuleSpecifier::parse(&format!("sandbox:///{target}"))
            .map_err(ModuleLoaderError::from_err)?;
        self.checked(&url)?;
        Ok(url)
    }

    fn source_for(
        &self,
        specifier: &ModuleSpecifier,
    ) -> std::result::Result<ModuleSource, ModuleLoaderError> {
        let path = self.checked(specifier)?;
        if is_reserved_builtin_path(&path) && !self.alias_allowed(&path) {
            return Err(ModuleLoaderError::generic(
                "reserved builtin is not available in this profile",
            ));
        }
        let text = self
            .plugin
            .source(&path)
            .ok_or_else(|| ModuleLoaderError::generic("module is outside approved graph"))?;
        Ok(ModuleSource::new(
            ModuleType::JavaScript,
            ModuleSourceCode::String(text.to_owned().into()),
            specifier,
            None,
        ))
    }
}

impl ModuleLoader for GraphLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> std::result::Result<ModuleSpecifier, ModuleLoaderError> {
        if (referrer == "." || referrer.is_empty())
            && specifier == format!("sandbox:///{}", self.plugin.entrypoint())
        {
            let url = ModuleSpecifier::parse(specifier).map_err(ModuleLoaderError::from_err)?;
            self.checked(&url)?;
            return Ok(url);
        }
        if builtin_alias_target(specifier).is_some() {
            return self.alias_module(specifier);
        }
        if !(specifier.starts_with("./") || specifier.starts_with("../")) {
            return Err(ModuleLoaderError::generic(
                "only relative graph imports are permitted",
            ));
        }
        let base = ModuleSpecifier::parse(referrer).map_err(ModuleLoaderError::from_err)?;
        let base_path = self.checked(&base)?;
        let path = denosanbox_core::resolve_relative(&base_path, specifier)
            .map_err(|e| ModuleLoaderError::generic(e.to_string()))?;
        let url = ModuleSpecifier::parse(&format!("sandbox:///{path}"))
            .map_err(ModuleLoaderError::from_err)?;
        self.checked(&url)?;
        Ok(url)
    }
    fn load(
        &self,
        specifier: &ModuleSpecifier,
        _referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        ModuleLoadResponse::Sync(self.source_for(specifier))
    }
}
struct DenoDriver {
    runtime: JsRuntime,
    executor: tokio::runtime::Runtime,
    control: Arc<Control>,
    plugin: PreparedPlugin,
    module: Option<ModuleId>,
    timeout: Duration,
    completed: u64,
    cancelled: u64,
    metrics: Arc<MetricsShared>,
    observer: Option<denosanbox_core::MetricsObserver>,
    heap_limit: Option<usize>,
}

impl DenoDriver {
    /// Publishes an exact snapshot for `DriverControl::metrics` and forwards it
    /// to the engine hook installed by `start_with`.
    fn publish_metrics(&self) {
        self.metrics
            .completed
            .store(self.completed, Ordering::Release);
        self.metrics
            .cancelled
            .store(self.cancelled, Ordering::Release);
        if let Some(observer) = &self.observer {
            observer(&self.metrics.snapshot(self.heap_limit));
        }
    }

    fn measure_heap(&mut self) {
        let statistics = self.runtime.v8_isolate().get_heap_statistics();
        self.metrics
            .set_heap(statistics.used_heap_size(), statistics.total_heap_size());
    }
}

impl Driver for DenoDriver {
    fn start_with(&mut self, options: StartOptions) -> Result<()> {
        self.observer = options.on_invocation;
        self.start()
    }
    fn start(&mut self) -> Result<()> {
        // Module files come only from the immutable prepared graph, which is
        // size-bounded and already parsed by preparation, so evaluation is
        // synchronous over in-memory imports. Only `block_on` is used here (no
        // nested `timeout`/`enter`), keeping startup within one runtime context.
        let _watchdog = Watchdog::new(self.control.clone(), self.timeout)?;
        let specifier = ModuleSpecifier::parse(&format!("sandbox:///{}", self.plugin.entrypoint()))
            .map_err(|e| Error::InvalidPlugin {
                reason: e.to_string(),
            })?;
        let control = self.control.clone();
        let runtime = &mut self.runtime;
        let module = self.executor.block_on(async {
            let module = runtime
                .load_main_es_module(&specifier)
                .await
                .map_err(|e| control.error(e))?;
            let _ = control;
            let evaluation = runtime.mod_evaluate(module);
            runtime
                .with_event_loop_future(evaluation, PollEventLoopOptions::default())
                .await
                .map_err(|e| self.control.error(e))?;
            Ok::<_, Error>(module)
        });
        let module = module?;
        self.control.running.store(false, Ordering::Release);
        self.module = Some(module);
        Ok(())
    }
    fn invoke(&mut self, invocation: Invocation) -> Result<Value> {
        // Deno may eagerly poll an async op while invoking the JS export.
        let _runtime_guard = self.executor.enter();
        if self.control.is_cancelled() {
            return Err(self.control.error("execution terminated"));
        }
        let timeout = invocation
            .deadline
            .map(|d| d.duration_since(SystemTime::now()).unwrap_or_default())
            .unwrap_or(invocation.timeout)
            .min(invocation.timeout);
        if timeout.is_zero() {
            return Err(Error::DeadlineExceeded);
        }
        let _watchdog = Watchdog::new(self.control.clone(), timeout)?;
        let module = self.module.ok_or_else(|| Error::Internal {
            message: "module not initialized".into(),
        })?;
        let namespace = self
            .runtime
            .get_module_namespace(module)
            .map_err(|e| self.control.error(e))?;
        let mut export = invocation.export;
        let runtime = &mut self.runtime;
        let (function, input) = {
            deno_core::scope!(scope, runtime);
            let namespace = v8::Local::new(scope, namespace);
            let mut value = lookup(scope, namespace, &export).ok_or_else(|| Error::Execution {
                message: "export missing".into(),
            })?;
            // Child files without an explicit ESM `onmessage` export dispatch
            // through the trusted global/handler fallback.
            if export == "onmessage" && !value.is_function() {
                export = "__denosanboxDispatchWorkerMessage".to_owned();
                value = lookup(scope, namespace, &export).ok_or_else(|| Error::Execution {
                    message: "export missing".into(),
                })?;
            }
            let function =
                v8::Local::<v8::Function>::try_from(value).map_err(|_| Error::Execution {
                    message: "export is not callable".into(),
                })?;
            let input = deno_core::serde_v8::to_v8(scope, invocation.input)
                .map_err(|e| self.control.error(e))?;
            (
                v8::Global::new(scope, function),
                v8::Global::new(scope, input),
            )
        };
        let result = self.runtime.call_with_args(&function, &[input]);
        let value = self
            .executor
            .block_on(async {
                tokio::time::timeout(
                    timeout,
                    self.runtime
                        .with_event_loop_future(result, PollEventLoopOptions::default()),
                )
                .await
            })
            .map_err(|_| {
                self.control.timed_out.store(true, Ordering::Release);
                self.control.terminate();
                Error::DeadlineExceeded
            })?
            .map_err(|e| self.control.error(e))?;
        let output = {
            deno_core::scope!(scope, &mut self.runtime);
            let value = v8::Local::new(scope, value);
            let json = v8::json::stringify(scope, value)
                .ok_or_else(|| self.control.error("result is not JSON serializable"))?;
            if json.utf8_length(scope) > BOOTSTRAP_MESSAGE_LIMIT {
                return Err(Error::LimitExceeded {
                    limit: "max_message_bytes".into(),
                    detail: "result exceeds 1MiB".into(),
                });
            }
            serde_json::from_str(&json.to_rust_string_lossy(scope)).unwrap_or(Value::Null)
        };
        self.completed += 1;
        self.measure_heap();
        self.publish_metrics();
        Ok(output)
    }
    fn close(self: Box<Self>) -> Result<CloseReport> {
        let mut report = CloseReport {
            invocations_completed: self.completed,
            invocations_cancelled: self.completed.saturating_add(self.cancelled),
            forced_termination: self.control.interrupted.load(Ordering::Acquire)
                || self.control.timed_out.load(Ordering::Acquire),
            closed_cleanly: true,
            pending_host_operations: self.control.host.outstanding(),
        };
        report.closed_cleanly = report.pending_host_operations == 0;
        *self.control.isolate.lock().unwrap() = None;
        drop(self);
        Ok(report)
    }
    fn metrics(&self) -> RuntimeMetrics {
        self.metrics.snapshot(self.heap_limit)
    }
}

fn lookup<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    namespace: v8::Local<'s, v8::Object>,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let key = v8::String::new(scope, name)?;
    namespace.get(scope, key.into())
}

fn decode_message(input: &str) -> std::result::Result<Value, ModuleLoaderError> {
    if input.len() > BOOTSTRAP_MESSAGE_LIMIT {
        return Err(ModuleLoaderError::generic(
            "serialized message exceeds 1MiB",
        ));
    }
    serde_json::from_str(input).map_err(|_| ModuleLoaderError::generic("invalid JSON message"))
}
fn guest_error(error: impl ToString) -> ModuleLoaderError {
    ModuleLoaderError::generic(error.to_string())
}
#[deno_core::op2]
#[serde]
fn op_sandbox_resource(
    state: &mut OpState,
    #[string] action: String,
    #[string] input: String,
) -> std::result::Result<serde_json::Value, ModuleLoaderError> {
    let input = decode_message(&input)?;
    let context = state.borrow::<Arc<denosanbox_core::HostContext>>().clone();
    let arg = |index: usize| {
        input
            .get(index)
            .and_then(Value::as_str)
            .ok_or_else(|| ModuleLoaderError::generic("expected string argument"))
    };
    match action.as_str() {
        "env_get" => Ok(context
            .env_get(arg(0)?)
            .map_err(guest_error)?
            .map(Value::String)
            .unwrap_or(Value::Null)),
        "env_set" => {
            context.env_set(arg(0)?, arg(1)?).map_err(guest_error)?;
            Ok(Value::Null)
        }
        "cwd" => Ok(Value::String(context.cwd().map_err(guest_error)?)),
        "exit" => {
            state.borrow::<Arc<Control>>().cancel();
            Err(ModuleLoaderError::generic("sandbox exited"))
        }
        _ => Err(ModuleLoaderError::generic("unsupported resource action")),
    }
}
#[deno_core::op2]
#[serde]
async fn op_sandbox_resource_async(
    state: Rc<RefCell<OpState>>,
    #[string] action: String,
    #[string] input: String,
) -> std::result::Result<serde_json::Value, ModuleLoaderError> {
    let input = decode_message(&input)?;
    let context = state
        .borrow()
        .borrow::<Arc<denosanbox_core::HostContext>>()
        .clone();
    let arg = |index: usize| {
        input
            .get(index)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ModuleLoaderError::generic("expected string argument"))
    };
    match action.as_str() {
        "read" => {
            let bytes = context.read_async(arg(0)?).await.map_err(guest_error)?;
            Ok(Value::String(String::from_utf8(bytes).map_err(|_| {
                ModuleLoaderError::generic("file is not UTF-8")
            })?))
        }
        "write" => {
            context
                .write_async(arg(0)?, arg(1)?.into_bytes())
                .await
                .map_err(guest_error)?;
            Ok(Value::Null)
        }
        "chdir" => {
            context.set_cwd_async(arg(0)?).await.map_err(guest_error)?;
            Ok(Value::Null)
        }
        _ => Err(ModuleLoaderError::generic("unsupported resource action")),
    }
}
#[deno_core::op2]
#[serde]
async fn op_sandbox_host(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] input: String,
) -> std::result::Result<serde_json::Value, ModuleLoaderError> {
    let input = decode_message(&input)?;
    let context = state
        .borrow()
        .borrow::<Arc<denosanbox_core::HostContext>>()
        .clone();
    context
        .call_versioned(1, name, input)
        .await
        .map_err(guest_error)
}
#[deno_core::op2]
#[serde]
async fn op_sandbox_worker(
    state: Rc<RefCell<OpState>>,
    #[string] action: String,
    #[string] input: String,
) -> std::result::Result<serde_json::Value, ModuleLoaderError> {
    let input = decode_message(&input)?;
    let context = state
        .borrow()
        .borrow::<Arc<denosanbox_core::HostContext>>()
        .clone();
    context
        .worker_operation(action, input)
        .await
        .map_err(guest_error)
}
#[deno_core::op2]
#[serde]
async fn op_sandbox_fetch(
    state: Rc<RefCell<OpState>>,
    #[string] input: String,
) -> std::result::Result<serde_json::Value, ModuleLoaderError> {
    let input = decode_message(&input)?;
    let request = serde_json::from_value(input)
        .map_err(|_| ModuleLoaderError::generic("invalid HTTP request"))?;
    let context = state
        .borrow()
        .borrow::<Arc<denosanbox_core::HostContext>>()
        .clone();
    serde_json::to_value(context.http_request(request).await.map_err(guest_error)?)
        .map_err(guest_error)
}
#[deno_core::op2(fast)]
fn op_sandbox_log(
    state: &mut OpState,
    #[string] text: String,
) -> std::result::Result<(), ModuleLoaderError> {
    let control = state.borrow::<Arc<Control>>();
    let context = state.borrow::<Arc<denosanbox_core::HostContext>>();
    // The effective context policy is read on every record, so a live
    // narrow (including to a smaller `max_log_bytes`) applies immediately.
    let limit = context.policy().limits().max_log_bytes();
    // A single record is bounded independent of the policy, before any
    // accounting touches the shared counter.
    if text.len() > MAX_LOG_RECORD_BYTES {
        return Err(ModuleLoaderError::generic("log record limit exceeded"));
    }
    let mut used = control.log_bytes.load(Ordering::Acquire);
    loop {
        let next = used
            .checked_add(text.len())
            .filter(|n| *n <= limit)
            .ok_or_else(|| ModuleLoaderError::generic("per-invocation log limit exceeded"))?;
        match control.log_bytes.compare_exchange_weak(
            used,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(()),
            Err(actual) => used = actual,
        }
    }
}
deno_core::extension!(sandbox_ops,
    ops=[op_sandbox_resource,op_sandbox_resource_async,op_sandbox_host,op_sandbox_worker,op_sandbox_fetch,op_sandbox_log],
    options={context:Arc<denosanbox_core::HostContext>,control:Arc<Control>},
    state=|state,options|{state.put(options.context);state.put(options.control);}
);
