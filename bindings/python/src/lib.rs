//! Native async bindings for the `denosanbox` facade.
//!
//! This module intentionally exposes thin, underscore-free data objects and
//! awaitables under `denosanbox._native`. The supported public Python surface
//! is the `denosanbox` package, which wraps these classes to translate errors
//! and to provide deterministic close/context-manager behaviour.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use denosanbox::{
    policy, BackendKind, CloseReport, Engine, EngineConfig, EngineLimits, Error, HostGrant,
    InvocationContext, InvocationOptions, PluginSource, Policy, PreparedPlugin, RegisteredCallback,
    SandboxHandle, SandboxOptions, ShutdownReport,
};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyAny;
use pyo3_async_runtimes::tokio::future_into_py;
use serde_json::Value;

create_exception!(
    _native,
    NativeError,
    PyException,
    "Internal error raised by the native denosanbox extension."
);

/// Builds the single native exception type used across the FFI boundary.
///
/// Every instance carries stable `kind` and `code` attributes. The Python
/// facade converts this internal type into the public `SandboxError` subclass
/// whose name matches the stable `Error::kind()` string.
fn native_error(kind: &'static str, message: String) -> PyErr {
    Python::with_gil(|py| {
        let err = NativeError::new_err(message);
        let value = err.value(py);
        let _ = value.setattr("kind", kind);
        let _ = value.setattr("code", kind);
        err
    })
}

fn error_to_py(err: Error) -> PyErr {
    native_error(err.kind(), err.to_string())
}

fn parse_backend(backend: &str) -> PyResult<BackendKind> {
    match backend {
        "deno" => Ok(BackendKind::Deno),
        "fake" => Ok(BackendKind::Fake),
        other => Err(native_error(
            "invalid_configuration",
            format!("unknown backend '{other}'; expected 'deno' or 'fake'"),
        )),
    }
}

struct PythonCallback {
    callback: Py<PyAny>,
    locals: pyo3_async_runtimes::TaskLocals,
}

/// Trusted invocation metadata. It is created by Rust, never guest JSON.
#[pyclass(module = "denosanbox._native", name = "InvocationContext")]
struct PyInvocationContext {
    inner: InvocationContext,
}

#[pymethods]
impl PyInvocationContext {
    #[getter]
    fn sandbox_id(&self) -> String {
        self.inner.sandbox_id().to_string()
    }
    #[getter]
    fn scope(&self) -> String {
        self.inner.scope().to_owned()
    }
    #[getter]
    fn cancelled(&self) -> bool {
        self.inner.cancellation().is_cancelled()
    }
    #[getter]
    fn remaining_ms(&self) -> Option<u64> {
        self.inner
            .deadline()
            .remaining()
            .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
    }
}

/// Owned engine handle.
#[pyclass(module = "denosanbox._native", name = "NativeEngine")]
struct NativeEngine {
    inner: Engine,
    callbacks: Mutex<BTreeMap<String, RegisteredCallback>>,
}

#[pymethods]
impl NativeEngine {
    #[new]
    #[pyo3(signature = (
        backend = "deno",
        *,
        max_active_sandboxes = None,
        max_pending_invocations = None,
        max_message_bytes = None,
        default_invocation_timeout_ms = None,
        max_invocation_timeout_ms = None,
        max_modules = None,
        max_module_bytes = None,
        max_graph_bytes = None,
        host_capabilities = None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        backend: &str,
        max_active_sandboxes: Option<usize>,
        max_pending_invocations: Option<usize>,
        max_message_bytes: Option<usize>,
        default_invocation_timeout_ms: Option<u64>,
        max_invocation_timeout_ms: Option<u64>,
        max_modules: Option<usize>,
        max_module_bytes: Option<usize>,
        max_graph_bytes: Option<usize>,
        host_capabilities: Option<Vec<String>>,
    ) -> PyResult<Self> {
        let backend = parse_backend(backend)?;
        let mut limits = EngineLimits::default();
        if let Some(value) = max_active_sandboxes {
            limits.max_active_sandboxes = value;
        }
        if let Some(value) = max_pending_invocations {
            limits.max_pending_invocations = value;
        }
        if let Some(value) = max_message_bytes {
            limits.max_message_bytes = value;
        }
        if let Some(value) = default_invocation_timeout_ms {
            limits.default_invocation_timeout = Duration::from_millis(value);
        }
        if let Some(value) = max_invocation_timeout_ms {
            limits.max_invocation_timeout = Duration::from_millis(value);
        }
        if let Some(value) = max_modules {
            limits.max_modules = value;
        }
        if let Some(value) = max_module_bytes {
            limits.max_module_bytes = value;
        }
        if let Some(value) = max_graph_bytes {
            limits.max_graph_bytes = value;
        }
        let mut ceiling = Policy::default();
        for name in host_capabilities.unwrap_or_default() {
            ceiling = ceiling.allow_capability(&name).map_err(|_| {
                native_error(
                    "invalid_configuration",
                    "invalid host capability name".into(),
                )
            })?;
        }
        let engine = Engine::with_policy(EngineConfig::new(backend).with_limits(limits), ceiling)
            .map_err(error_to_py)?;
        Ok(Self {
            inner: engine,
            callbacks: Mutex::new(BTreeMap::new()),
        })
    }

    /// Validates and freezes a plugin source graph while the GIL is released.
    #[pyo3(signature = (source, entrypoint = "main.js".to_string()))]
    fn prepare(
        &self,
        py: Python<'_>,
        source: String,
        entrypoint: String,
    ) -> PyResult<Py<NativePreparedPlugin>> {
        let engine = self.inner.clone();
        let plugin_source = PluginSource::single(entrypoint, source);
        let prepared = py
            .allow_threads(move || engine.prepare(plugin_source))
            .map_err(error_to_py)?;
        Py::new(py, NativePreparedPlugin { inner: prepared })
    }

    /// Creates an independent sandbox. Returns an awaitable resolving to a
    /// `NativeSandbox`.
    #[pyo3(signature = (
        prepared,
        *,
        default_timeout_ms = None,
        memory_limit_bytes = None,
        host_grants = None
    ))]
    fn create<'py>(
        &self,
        py: Python<'py>,
        prepared: PyRef<'_, NativePreparedPlugin>,
        default_timeout_ms: Option<u64>,
        memory_limit_bytes: Option<usize>,
        host_grants: Option<Vec<(String, String)>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let engine = self.inner.clone();
        let plugin = prepared.inner.clone();
        let max_message_bytes = engine.limits().max_message_bytes;
        let mut requested = Policy::default();
        let mut grants = Vec::new();
        {
            let registered = self
                .callbacks
                .lock()
                .map_err(|_| native_error("internal", "callback registry unavailable".into()))?;
            for (name, scope) in host_grants.unwrap_or_default() {
                let callback = registered
                    .get(&name)
                    .ok_or_else(|| {
                        native_error("invalid_configuration", "unknown host callback".into())
                    })?
                    .clone();
                requested = requested.allow_capability(&name).map_err(|_| {
                    native_error("invalid_configuration", "invalid capability name".into())
                })?;
                grants.push(HostGrant { callback, scope });
            }
        }
        let options = SandboxOptions {
            default_invocation_timeout: default_timeout_ms.map(Duration::from_millis),
            memory_limit_bytes,
            policy: requested,
            host_grants: grants,
            ..SandboxOptions::default()
        };

        future_into_py(py, async move {
            // Run creation in a detached task and await its JoinHandle. If the
            // Python awaitable is dropped mid-create, dropping a JoinHandle
            // does not abort the task: it still either returns the handle (and
            // immediately drops it, whose Drop requests close) or fails. This
            // prevents an orphaned sandbox/worker from escaping cancellation.
            let task = tokio::spawn(async move { engine.create(plugin, options).await });
            let sandbox = task
                .await
                .map_err(|err| {
                    native_error("internal", format!("sandbox creation task failed: {err}"))
                })?
                .map_err(error_to_py)?;
            Python::with_gil(|py| {
                Py::new(
                    py,
                    NativeSandbox {
                        inner: Arc::new(sandbox),
                        max_message_bytes,
                    },
                )
            })
        })
    }

    /// Capture the running Python loop; callback bodies execute on that loop.
    #[pyo3(signature = (name, callback, input_limit = 1048576, output_limit = 1048576))]
    fn register_host(
        &self,
        py: Python<'_>,
        name: String,
        callback: Py<PyAny>,
        input_limit: usize,
        output_limit: usize,
    ) -> PyResult<()> {
        if !callback.bind(py).is_callable() {
            return Err(native_error(
                "invalid_configuration",
                "host callback must be callable".into(),
            ));
        }
        let handler = Arc::new(PythonCallback {
            callback,
            locals: pyo3_async_runtimes::tokio::get_current_locals(py)?,
        });
        let registered = self
            .inner
            .register_host_async(&name, input_limit, output_limit, move |context, input| {
                let handler = handler.clone();
                async move {
                    context.check_live()?;
                    let future = Python::with_gil(|py| {
                        let context_object = Py::new(
                            py,
                            PyInvocationContext {
                                inner: context.clone(),
                            },
                        )?;
                        let input = std::str::from_utf8(&input).map_err(|_| {
                            pyo3::exceptions::PyValueError::new_err("invalid callback JSON")
                        })?;
                        let awaitable = handler.callback.bind(py).call1((context_object, input))?;
                        pyo3_async_runtimes::into_future_with_locals(&handler.locals, awaitable)
                    })
                    .map_err(|_| {
                        policy::Error::invalid_argument("Python callback could not be scheduled")
                    })?;
                    let result = future
                        .await
                        .map_err(|_| policy::Error::invalid_argument("Python callback failed"))?;
                    context.check_live()?;
                    Python::with_gil(|py| {
                        let output: String = result.extract(py).map_err(|_| {
                            policy::Error::invalid_argument("Python callback must return JSON text")
                        })?;
                        if output.len() > output_limit {
                            return Err(policy::Error::limit_exceeded(
                                "Python callback output exceeds limit",
                            ));
                        }
                        Ok(output.into_bytes())
                    })
                }
            })
            .map_err(error_to_py)?;
        self.callbacks
            .lock()
            .map_err(|_| native_error("internal", "callback registry unavailable".into()))?
            .insert(name, registered);
        Ok(())
    }

    /// Closes every sandbox owned by this engine.
    fn shutdown<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let engine = self.inner.clone();
        future_into_py(py, async move {
            let report = engine.shutdown().await.map_err(error_to_py)?;
            Python::with_gil(|py| Py::new(py, PyShutdownReport::from(report)))
        })
    }

    #[getter]
    fn backend(&self) -> &'static str {
        self.inner.backend().as_str()
    }

    #[getter]
    fn active_sandboxes(&self) -> usize {
        self.inner.active_sandboxes()
    }

    fn __repr__(&self) -> String {
        format!(
            "<denosanbox._native.NativeEngine backend={:?} active_sandboxes={}>",
            self.inner.backend().as_str(),
            self.inner.active_sandboxes()
        )
    }
}

/// Immutable prepared plugin graph.
#[pyclass(module = "denosanbox._native", name = "NativePreparedPlugin")]
struct NativePreparedPlugin {
    inner: PreparedPlugin,
}

#[pymethods]
impl NativePreparedPlugin {
    #[getter]
    fn entrypoint(&self) -> &str {
        self.inner.entrypoint()
    }

    #[getter]
    fn module_count(&self) -> usize {
        self.inner.module_count()
    }

    #[getter]
    fn total_bytes(&self) -> usize {
        self.inner.total_bytes()
    }

    fn __repr__(&self) -> String {
        format!(
            "<denosanbox._native.NativePreparedPlugin entrypoint={:?} modules={} bytes={}>",
            self.inner.entrypoint(),
            self.inner.module_count(),
            self.inner.total_bytes()
        )
    }
}

/// Drops the reference to an in-flight call. If the Rust future is dropped
/// before completion (for example because the Python task was cancelled), the
/// guard requests terminal cancellation on the sandbox.
struct CancelGuard {
    sandbox: Option<Arc<SandboxHandle>>,
}

impl CancelGuard {
    fn new(sandbox: Arc<SandboxHandle>) -> Self {
        Self {
            sandbox: Some(sandbox),
        }
    }

    fn disarm(mut self) {
        self.sandbox = None;
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Some(sandbox) = self.sandbox.take() {
            sandbox.cancel();
        }
    }
}

/// Handle to one independent sandbox.
#[pyclass(module = "denosanbox._native", name = "NativeSandbox")]
struct NativeSandbox {
    inner: Arc<SandboxHandle>,
    max_message_bytes: usize,
}

#[pymethods]
impl NativeSandbox {
    /// Calls a named ESM export with a JSON input string. Returns an awaitable
    /// resolving to the JSON-encoded result string.
    #[pyo3(signature = (export, input_json, timeout_ms = None))]
    fn call<'py>(
        &self,
        py: Python<'py>,
        export: &str,
        input_json: &str,
        timeout_ms: Option<u64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if input_json.len() > self.max_message_bytes {
            return Err(native_error(
                "limit_exceeded",
                format!("input_json exceeds {} bytes", self.max_message_bytes),
            ));
        }
        // Bound the source before allocating a parsed tree, and release the
        // GIL while parsing. The core still validates the serialized value.
        let input: Value = py
            .allow_threads(|| serde_json::from_str(input_json))
            .map_err(|err| {
                native_error(
                    "invalid_configuration",
                    format!("input_json is not valid JSON: {err}"),
                )
            })?;
        let export = export.to_string();
        let sandbox = self.inner.clone();

        future_into_py(py, async move {
            let mut options = InvocationOptions::default();
            if let Some(ms) = timeout_ms {
                options.timeout = Some(Duration::from_millis(ms));
            }
            let guard = CancelGuard::new(sandbox.clone());
            let result = sandbox.call(&export, input, options).await;
            // Every completed call, successful or failed, disarms the guard;
            // only an externally dropped future requests cancellation.
            guard.disarm();
            match result {
                Ok(value) => serde_json::to_string(&value).map_err(|err| {
                    native_error(
                        "internal",
                        format!("failed to encode result as JSON: {err}"),
                    )
                }),
                Err(err) => Err(error_to_py(err)),
            }
        })
    }

    /// Requests terminal cancellation of the current invocation. The sandbox
    /// cannot be used for later calls.
    fn cancel(&self) {
        self.inner.cancel();
    }

    /// Requests close and joins the worker. The returned awaitable resolves to
    /// a `CloseReport`.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sandbox = self.inner.clone();
        future_into_py(py, async move {
            let report = sandbox.close().await.map_err(error_to_py)?;
            Python::with_gil(|py| Py::new(py, PyCloseReport::from(report)))
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "<denosanbox._native.NativeSandbox at {:p}>",
            self.inner.as_ref() as *const SandboxHandle
        )
    }
}

/// Public close report fields.
#[pyclass(module = "denosanbox._native", name = "CloseReport")]
#[derive(Clone)]
struct PyCloseReport {
    #[pyo3(get)]
    closed_cleanly: bool,
    #[pyo3(get)]
    forced_termination: bool,
    #[pyo3(get)]
    invocations_completed: u64,
    #[pyo3(get)]
    invocations_cancelled: u64,
}

impl From<CloseReport> for PyCloseReport {
    fn from(report: CloseReport) -> Self {
        Self {
            closed_cleanly: report.closed_cleanly,
            forced_termination: report.forced_termination,
            invocations_completed: report.invocations_completed,
            invocations_cancelled: report.invocations_cancelled,
        }
    }
}

#[pymethods]
impl PyCloseReport {
    fn __repr__(&self) -> String {
        format!(
            "<denosanbox.CloseReport closed_cleanly={} forced_termination={} \
             invocations_completed={} invocations_cancelled={}>",
            self.closed_cleanly,
            self.forced_termination,
            self.invocations_completed,
            self.invocations_cancelled
        )
    }
}

/// Public shutdown report fields.
#[pyclass(module = "denosanbox._native", name = "ShutdownReport")]
#[derive(Clone)]
struct PyShutdownReport {
    #[pyo3(get)]
    sandboxes_closed: usize,
    #[pyo3(get)]
    sandboxes_forced: usize,
    #[pyo3(get)]
    clean: bool,
}

impl From<ShutdownReport> for PyShutdownReport {
    fn from(report: ShutdownReport) -> Self {
        Self {
            sandboxes_closed: report.sandboxes_closed,
            sandboxes_forced: report.sandboxes_forced,
            clean: report.clean,
        }
    }
}

#[pymethods]
impl PyShutdownReport {
    fn __repr__(&self) -> String {
        format!(
            "<denosanbox.ShutdownReport sandboxes_closed={} sandboxes_forced={} clean={}>",
            self.sandboxes_closed, self.sandboxes_forced, self.clean
        )
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add("NativeError", py.get_type::<NativeError>())?;
    m.add_class::<PyInvocationContext>()?;
    m.add_class::<NativeEngine>()?;
    m.add_class::<NativePreparedPlugin>()?;
    m.add_class::<NativeSandbox>()?;
    m.add_class::<PyCloseReport>()?;
    m.add_class::<PyShutdownReport>()?;
    Ok(())
}
