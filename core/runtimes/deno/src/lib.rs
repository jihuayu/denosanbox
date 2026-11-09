//! Thread-affine in-process deno_core/V8 backend. No guest operating-system ops.
use deno_core::error::ModuleLoaderError;
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
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, SystemTime};

#[derive(Default)]
struct Control {
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
        if self.timed_out.load(Ordering::Acquire) {
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
        self.cancelled.store(true, Ordering::Release);
        self.terminate();
    }
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.timed_out.load(Ordering::Acquire)
    }
}
struct Watchdog {
    done: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Watchdog {
    fn new(control: Arc<Control>, timeout: Duration) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("denosanbox-watchdog".into())
            .spawn(move || {
                if matches!(
                    rx.recv_timeout(timeout),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    control.timed_out.store(true, Ordering::Release);
                    control.terminate();
                }
            })
            .map_err(|e| Error::Backend {
                message: e.to_string(),
            })?;
        Ok(Self {
            done: Some(tx),
            thread: Some(thread),
        })
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        self.done.take();
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
    fn create(&self, plugin: PreparedPlugin, options: &SandboxOptions) -> Result<CreatedDriver> {
        let control = Arc::new(Control::default());
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
                    create_params: options
                        .memory_limit_bytes
                        .map(|bytes| v8::CreateParams::default().heap_limits(0, bytes)),
                    ..Default::default()
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
                delete globalThis.Deno;
                delete globalThis.__bootstrap;
                delete globalThis.SharedArrayBuffer;
                delete globalThis.Atomics;
                delete globalThis.WebAssembly;
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
            deno_core::serde_v8::from_v8(scope, value).map_err(|e| self.control.error(e))?
        };
        self.completed += 1;
        Ok(output)
    }
    fn close(self: Box<Self>) -> Result<CloseReport> {
        let report = CloseReport {
            invocations_completed: self.completed,
            invocations_cancelled: 0,
            forced_termination: self.control.is_cancelled(),
            closed_cleanly: true,
        };
        *self.control.isolate.lock().unwrap() = None;
        drop(self);
        Ok(report)
    }
}
