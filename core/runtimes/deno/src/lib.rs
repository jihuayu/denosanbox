//! Thread-affine in-process deno_core/V8 backend. No guest operating-system ops.
mod buffers;
use deno_core::error::ModuleLoaderError;
use deno_core::OpState;
use deno_core::{
    v8, JsRuntime, ModuleId, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse,
    ModuleLoader, ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType,
    PollEventLoopOptions, ResolutionKind, RuntimeOptions,
};
use denosanbox_core::{
    BackendKind, CloseReport, CreatedDriver, Driver, DriverControl, DriverFactory, Error,
    Invocation, PreparedPlugin, Result, SandboxOptions,
};
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, SystemTime};

struct Control {
    host: Arc<denosanbox_core::HostContext>,
    running: AtomicBool,
    interrupted: AtomicBool,
    heap_exceeded: AtomicBool,
    log_bytes: std::sync::atomic::AtomicUsize,
    cancelled: AtomicBool,
    timed_out: AtomicBool,
    isolate: Mutex<Option<v8::IsolateHandle>>,
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
        let control = Arc::new(Control {
            host: context.clone(),
            running: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            heap_exceeded: AtomicBool::new(false),
            log_bytes: std::sync::atomic::AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            isolate: Mutex::new(None),
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
                let mut runtime = JsRuntime::new(RuntimeOptions {
                    module_loader: Some(Rc::new(GraphLoader(plugin.clone()))),
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
                // Remove the trusted core entry points before loading any guest source.
                runtime
                    .execute_script(
                        "sandbox-bootstrap.js",
                        r#"
                (()=>{
                  const ops=Deno.core.ops;
                  const stringify=JSON.stringify;
                  const encode=(value)=>{const text=stringify(value);if(typeof text!=='string'||text.length>1048576)throw new RangeError('message limit exceeded');return text;};
                  const resource=(name,...args)=>ops.op_sandbox_resource(name,encode(args));
                  const api=Object.freeze({
                    fs:Object.freeze({readTextFile:(path)=>ops.op_sandbox_resource_async('read',encode([path])),writeTextFile:(path,data)=>ops.op_sandbox_resource_async('write',encode([path,data]))}),
                    env:Object.freeze({get:(name)=>resource('env_get',name),set:(name,value)=>resource('env_set',name,value)}),
                    host:Object.freeze({protocolVersion:1,call:(name,input)=>ops.op_sandbox_host(name,encode(input))}),
                    cwd:()=>resource('cwd'),chdir:(path)=>ops.op_sandbox_resource_async('chdir',encode([path])),exit:()=>resource('exit')
                  });
                  Object.defineProperty(globalThis,'sandbox',{value:api,writable:false,configurable:false});
                })();
                (()=>{
                  const op=Deno.core.ops.op_sandbox_fetch;const stringify=JSON.stringify;
                  const fetch=async(url,init={})=>{
                    if(typeof url!=='string')throw new TypeError('fetch supports a URL string');
                    const text=init.body===undefined?'':String(init.body);
                    if(text.length>1048576)throw new RangeError('request body exceeds limit');
                    const encoded=unescape(encodeURIComponent(text));const body=Array.from(encoded,c=>c.charCodeAt(0));
                    const input=stringify({url,method:init.method||'GET',headers:init.headers||{},body});
                    if(input.length>1048576)throw new RangeError('request exceeds limit');
                    const response=await op(input);const bytes=Uint8Array.from(response.body);
                    const decode=()=>decodeURIComponent(escape(Array.from(bytes,c=>String.fromCharCode(c)).join('')));
                    return Object.freeze({status:response.status,ok:response.status>=200&&response.status<300,url:response.url,
                      headers:Object.freeze({get:(name)=>response.headers[String(name).toLowerCase()]??null}),
                      text:async()=>decode(),json:async()=>JSON.parse(decode()),arrayBuffer:async()=>bytes.buffer});
                  };
                  Object.defineProperty(globalThis,'fetch',{value:fetch,writable:false,configurable:false});
                })();
                delete globalThis.__bootstrap;
                (()=>{const stringify=String;const log=Deno.core.ops.op_sandbox_log;const emit=(...args)=>{const text=args.map(arg=>stringify(arg)).join(' ');if(text.length>65536)throw new RangeError('log limit exceeded');log(text);};Object.defineProperty(globalThis,'console',{value:Object.freeze({log:emit,info:emit,warn:emit,error:emit,debug:emit}),writable:false,configurable:false});})();
                delete globalThis.SharedArrayBuffer;
                delete globalThis.Atomics;
                delete globalThis.WebAssembly;
                delete globalThis.Deno;
            "#,
                    )
                    .map_err(|e| thread_control.error(e))?;
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
                }) as Box<dyn Driver>)
            }),
        })
    }
}
struct GraphLoader(PreparedPlugin);
impl GraphLoader {
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
        if self.0.source(path).is_none() {
            return Err(ModuleLoaderError::generic(
                "module is outside approved graph",
            ));
        }
        Ok(path.to_owned())
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
            && specifier == format!("sandbox:///{}", self.0.entrypoint())
        {
            let url = ModuleSpecifier::parse(specifier).map_err(ModuleLoaderError::from_err)?;
            self.checked(&url)?;
            return Ok(url);
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
        ModuleLoadResponse::Sync(self.checked(specifier).map(|path| {
            ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String(self.0.source(&path).unwrap().to_owned().into()),
                specifier,
                None,
            )
        }))
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
}
impl Driver for DenoDriver {
    fn start(&mut self) -> Result<()> {
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
            let evaluation = runtime.mod_evaluate(module);
            tokio::time::timeout(
                self.timeout,
                runtime.with_event_loop_future(evaluation, PollEventLoopOptions::default()),
            )
            .await
            .map_err(|_| Error::DeadlineExceeded)?
            .map_err(|e| control.error(e))?;
            Ok::<_, Error>(module)
        })?;
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
        let (function, input) = {
            deno_core::scope!(scope, &mut self.runtime);
            let namespace = v8::Local::new(scope, namespace);
            let key = v8::String::new(scope, &invocation.export).ok_or_else(|| Error::Backend {
                message: "export allocation failed".into(),
            })?;
            let value = namespace
                .get(scope, key.into())
                .ok_or_else(|| Error::Execution {
                    message: "export missing".into(),
                })?;
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
            if json.utf8_length(scope) > 1024 * 1024 {
                return Err(Error::LimitExceeded {
                    limit: "max_message_bytes".into(),
                    detail: "result exceeds 1MiB".into(),
                });
            }
            serde_json::from_str(&json.to_rust_string_lossy(scope))
                .map_err(|e| self.control.error(e))?
        };
        self.completed += 1;
        Ok(output)
    }
    fn close(self: Box<Self>) -> Result<CloseReport> {
        let mut report = CloseReport {
            invocations_completed: self.completed,
            invocations_cancelled: 0,
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
}
fn decode_message(input: &str) -> std::result::Result<Value, ModuleLoaderError> {
    if input.len() > 1024 * 1024 {
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
    let mut used = control.log_bytes.load(Ordering::Acquire);
    loop {
        let next = used
            .checked_add(text.len())
            .filter(|n| *n <= 65536)
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
