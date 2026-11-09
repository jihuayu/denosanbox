//! Native async bindings for the `denosanbox` facade.
//!
//! This module intentionally exposes thin, underscore-free data objects and
//! awaitables under `denosanbox._native`. The supported public Python surface
//! is the `denosanbox` package, which wraps these classes to translate errors
//! and to provide deterministic close/context-manager behaviour.
//!
//! The binding owns no accounting, policy or filesystem logic: module graphs,
//! policies, resource limits, mounts and close reports all come from the public
//! Rust facade. This file only converts host-supplied Python values into facade
//! inputs and facade outputs into typed Python data.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use denosanbox::{
    modules::CompatibilityProfile, policy, BackendKind, CloseReport, Engine, EngineConfig,
    EngineLimits, Error, HostGrant, InvocationContext, InvocationOptions, MountAccess,
    PluginSource, Policy, PreparedPlugin, RegisteredCallback, SandboxHandle, SandboxOptions,
    ShutdownReport,
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

/// Maps the two explicit facade profiles; no profile is ever auto-enabled.
fn parse_profile(profile: &str) -> PyResult<CompatibilityProfile> {
    match profile {
        "javascript-only" => Ok(CompatibilityProfile::JavaScriptOnly),
        "pure-js-npm" => Ok(CompatibilityProfile::PureJsNpm),
        other => Err(native_error(
            "invalid_configuration",
            format!("unknown profile '{other}'; expected 'javascript-only' or 'pure-js-npm'"),
        )),
    }
}

fn parse_mount_access(access: &str) -> PyResult<MountAccess> {
    match access {
        "read" => Ok(MountAccess::ReadOnly),
        "read-write" => Ok(MountAccess::ReadWrite),
        other => Err(native_error(
            "invalid_configuration",
            format!("unknown mount access '{other}'; expected 'read' or 'read-write'"),
        )),
    }
}

fn registry_unavailable() -> PyErr {
    native_error("internal", "callback registry unavailable".into())
}

struct PythonCallback {
    callback: Py<PyAny>,
    locals: pyo3_async_runtimes::TaskLocals,
}

/// Converts an optional host-granted capability list into a requested policy.
fn requested_policy(capabilities: &[String]) -> PyResult<Policy> {
    let mut requested = Policy::default();
    for name in capabilities {
        requested = requested.allow_capability(name).map_err(|_| {
            native_error(
                "invalid_configuration",
                format!("invalid capability name '{name}'"),
            )
        })?;
    }
    Ok(requested)
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
    /// Numeric limits of the immutable trusted creation ceiling. Per-sandbox
    /// numeric defaults are filled from here, never from `Limits::default()`,
    /// so an unspecified `SandboxOptions` value can never exceed the ceiling.
    /// Capabilities and read/write roots are deliberately *not* inherited.
    ceiling_limits: policy::Limits,
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
        host_capabilities = None,
        host_read_roots = None,
        host_write_roots = None,
        host_heap_target_bytes = None,
        host_memory_budget_bytes = None,
        host_max_sandboxes = None,
        host_max_message_bytes = None,
        host_deadline_ms = None,
        host_queue_depth = None,
        host_max_log_bytes = None
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
        host_read_roots: Option<Vec<String>>,
        host_write_roots: Option<Vec<String>>,
        host_heap_target_bytes: Option<usize>,
        host_memory_budget_bytes: Option<usize>,
        host_max_sandboxes: Option<usize>,
        host_max_message_bytes: Option<usize>,
        host_deadline_ms: Option<u64>,
        host_queue_depth: Option<usize>,
        host_max_log_bytes: Option<usize>,
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

        // The engine ceiling is trusted host configuration and may grant the
        // explicit capabilities/roots listed here. It still starts empty (no
        // implicit grants); per-sandbox requests are narrowed against it by
        // `Engine::with_policy`/`create`, which keeps authority monotonic.
        let mut ceiling = Policy::default();
        for name in host_capabilities.unwrap_or_default() {
            ceiling = ceiling.allow_capability(&name).map_err(|_| {
                native_error(
                    "invalid_configuration",
                    format!("invalid host capability name '{name}'"),
                )
            })?;
        }
        for root in host_read_roots.unwrap_or_default() {
            ceiling = ceiling.allow_read(&root).map_err(|_| {
                native_error(
                    "invalid_configuration",
                    format!("invalid read root '{root}'"),
                )
            })?;
        }
        for root in host_write_roots.unwrap_or_default() {
            ceiling = ceiling.allow_write(&root).map_err(|_| {
                native_error(
                    "invalid_configuration",
                    format!("invalid write root '{root}'"),
                )
            })?;
        }
        let mut policy_limits = policy::Limits::default();
        if let Some(value) = host_heap_target_bytes {
            policy_limits = policy_limits.with_heap_target_bytes(value);
        }
        if let Some(value) = host_memory_budget_bytes {
            policy_limits = policy_limits.with_memory_budget_bytes(value);
        }
        if let Some(value) = host_max_sandboxes {
            policy_limits = policy_limits.with_max_sandboxes(value);
        }
        if let Some(value) = host_max_message_bytes {
            policy_limits = policy_limits.with_max_message_bytes(value);
        }
        if let Some(value) = host_deadline_ms {
            policy_limits = policy_limits.with_deadline(Duration::from_millis(value));
        }
        if let Some(value) = host_queue_depth {
            policy_limits = policy_limits.with_queue_depth(value);
        }
        if let Some(value) = host_max_log_bytes {
            policy_limits = policy_limits.with_max_log_bytes(value);
        }
        ceiling = ceiling.with_limits(policy_limits);
        let ceiling_limits = ceiling.limits();

        let engine = Engine::with_policy(EngineConfig::new(backend).with_limits(limits), ceiling)
            .map_err(error_to_py)?;
        Ok(Self {
            inner: engine,
            callbacks: Mutex::new(BTreeMap::new()),
            ceiling_limits,
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

    /// Validates and freezes a host-supplied multi-module graph.
    ///
    /// `modules` maps root-relative paths (`.js`, `.mjs`, `.ts`, `.mts`) to
    /// source text; `entrypoint` must be one of its keys. Static imports,
    /// dynamic imports, re-exports and a `new Worker()` module graph resolve
    /// only inside this immutable graph. `profile` selects the explicit facade
    /// profile (`"javascript-only"` unless the host asks for `"pure-js-npm"`);
    /// no profile enables guest privileges by itself.
    #[pyo3(signature = (modules, entrypoint = "main.js".to_string(), profile = None))]
    fn prepare_modules(
        &self,
        py: Python<'_>,
        modules: Vec<(String, String)>,
        entrypoint: String,
        profile: Option<String>,
    ) -> PyResult<Py<NativePreparedPlugin>> {
        let profile = match profile.as_deref() {
            None => CompatibilityProfile::JavaScriptOnly,
            Some(name) => parse_profile(name)?,
        };
        let source = compose_source(modules, entrypoint, "modules")?;
        let engine = self.inner.clone();
        let prepared = py
            .allow_threads(move || engine.prepare_with_profile(source, profile))
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
        host_grants = None,
        capabilities = None,
        read_roots = None,
        write_roots = None,
        deadline_ms = None,
        queue_depth = None,
        max_message_bytes = None,
        max_log_bytes = None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn create<'py>(
        &self,
        py: Python<'py>,
        prepared: PyRef<'_, NativePreparedPlugin>,
        default_timeout_ms: Option<u64>,
        memory_limit_bytes: Option<usize>,
        host_grants: Option<Vec<(String, String)>>,
        capabilities: Option<Vec<String>>,
        read_roots: Option<Vec<String>>,
        write_roots: Option<Vec<String>>,
        deadline_ms: Option<u64>,
        queue_depth: Option<usize>,
        max_message_bytes: Option<usize>,
        max_log_bytes: Option<usize>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let engine = self.inner.clone();
        let plugin = prepared.inner.clone();
        let engine_max_message_bytes = engine.limits().max_message_bytes;
        let mut requested = requested_policy(&capabilities.unwrap_or_default())?;
        for root in read_roots.unwrap_or_default() {
            requested = requested.allow_read(&root).map_err(|_| {
                native_error(
                    "invalid_configuration",
                    format!("invalid read root '{root}'"),
                )
            })?;
        }
        for root in write_roots.unwrap_or_default() {
            requested = requested.allow_write(&root).map_err(|_| {
                native_error(
                    "invalid_configuration",
                    format!("invalid write root '{root}'"),
                )
            })?;
        }
        // Fill unspecified numeric options from the trusted creation ceiling.
        // The core validates every explicit override against that ceiling.
        let mut policy_limits = self.ceiling_limits;
        let max_message_bytes = max_message_bytes
            .unwrap_or(engine_max_message_bytes.min(self.ceiling_limits.max_message_bytes()));
        if let Some(ms) = deadline_ms {
            policy_limits = policy_limits.with_deadline(Duration::from_millis(ms));
        }
        if let Some(depth) = queue_depth {
            policy_limits = policy_limits.with_queue_depth(depth);
        }
        policy_limits = policy_limits.with_max_message_bytes(max_message_bytes);
        if let Some(bytes) = max_log_bytes {
            policy_limits = policy_limits.with_max_log_bytes(bytes);
        }
        requested = requested.with_limits(policy_limits);
        let mut grants = Vec::new();
        {
            let registered = self.callbacks.lock().map_err(|_| registry_unavailable())?;
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
            .map_err(|_| registry_unavailable())?
            .insert(name, registered);
        Ok(())
    }

    /// Attaches a trusted host directory to an already-created sandbox.
    ///
    /// The facade owns the directory capability, no-follow checks and policy
    /// intersection; this wrapper only forwards the host's explicit prefix,
    /// path and access mode. Guest input must never reach these arguments.
    fn mount_directory(
        &self,
        sandbox: PyRef<'_, NativeSandbox>,
        prefix: String,
        path: String,
        access: &str,
    ) -> PyResult<()> {
        let access = parse_mount_access(access)?;
        sandbox
            .inner
            .mount_directory(&prefix, std::path::Path::new(&path), access)
            .map_err(error_to_py)
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

    #[getter]
    fn limits(&self) -> PyEngineLimits {
        PyEngineLimits::from(self.inner.limits())
    }

    fn __repr__(&self) -> String {
        format!(
            "<denosanbox._native.NativeEngine backend={:?} active_sandboxes={}>",
            self.inner.backend().as_str(),
            self.inner.active_sandboxes()
        )
    }
}

/// Converts a Python mapping (or `(key, value)` sequence) to a facade graph.
fn compose_source(
    modules: Vec<(String, String)>,
    entrypoint: String,
    label: &str,
) -> PyResult<PluginSource> {
    let mut graph = BTreeMap::new();
    for (path, text) in modules {
        if graph.insert(path.clone(), text).is_some() {
            return Err(native_error(
                "invalid_plugin",
                format!("{label} contains duplicate module path '{path}'"),
            ));
        }
    }
    Ok(PluginSource {
        entrypoint,
        modules: graph,
    })
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

    /// Explicit compatibility profile recorded by the facade.
    #[getter]
    fn profile(&self) -> &'static str {
        self.inner.profile().as_str()
    }

    /// Sorted root-relative module paths in the immutable graph.
    #[getter]
    fn module_paths(&self) -> Vec<String> {
        self.inner
            .iter_modules()
            .map(|(path, _)| path.to_string())
            .collect()
    }

    fn __repr__(&self) -> String {
        format!(
            "<denosanbox._native.NativePreparedPlugin entrypoint={:?} modules={} bytes={} profile={:?}>",
            self.inner.entrypoint(),
            self.inner.module_count(),
            self.inner.total_bytes(),
            self.inner.profile().as_str()
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

    #[getter]
    fn id(&self) -> String {
        self.inner.id().to_string()
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
    #[pyo3(get)]
    pending_host_operations: usize,
}

impl From<CloseReport> for PyCloseReport {
    fn from(report: CloseReport) -> Self {
        Self {
            closed_cleanly: report.closed_cleanly,
            forced_termination: report.forced_termination,
            invocations_completed: report.invocations_completed,
            invocations_cancelled: report.invocations_cancelled,
            pending_host_operations: report.pending_host_operations,
        }
    }
}

#[pymethods]
impl PyCloseReport {
    fn __repr__(&self) -> String {
        format!(
            "<denosanbox.CloseReport closed_cleanly={} forced_termination={} \
             invocations_completed={} invocations_cancelled={} pending_host_operations={}>",
            self.closed_cleanly,
            self.forced_termination,
            self.invocations_completed,
            self.invocations_cancelled,
            self.pending_host_operations
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

/// Read-only view of the facade's engine limits.
#[pyclass(module = "denosanbox._native", name = "EngineLimits")]
#[derive(Clone)]
struct PyEngineLimits {
    #[pyo3(get)]
    max_active_sandboxes: usize,
    #[pyo3(get)]
    max_pending_invocations: usize,
    #[pyo3(get)]
    max_message_bytes: usize,
    #[pyo3(get)]
    default_invocation_timeout_ms: u64,
    #[pyo3(get)]
    max_invocation_timeout_ms: u64,
    #[pyo3(get)]
    max_modules: usize,
    #[pyo3(get)]
    max_module_bytes: usize,
    #[pyo3(get)]
    max_graph_bytes: usize,
}

impl From<&EngineLimits> for PyEngineLimits {
    fn from(limits: &EngineLimits) -> Self {
        Self {
            max_active_sandboxes: limits.max_active_sandboxes,
            max_pending_invocations: limits.max_pending_invocations,
            max_message_bytes: limits.max_message_bytes,
            default_invocation_timeout_ms: limits.default_invocation_timeout.as_millis() as u64,
            max_invocation_timeout_ms: limits.max_invocation_timeout.as_millis() as u64,
            max_modules: limits.max_modules,
            max_module_bytes: limits.max_module_bytes,
            max_graph_bytes: limits.max_graph_bytes,
        }
    }
}

#[pymethods]
impl PyEngineLimits {
    fn __repr__(&self) -> String {
        format!(
            "<denosanbox.EngineLimits max_active_sandboxes={} max_message_bytes={} \
             default_invocation_timeout_ms={} max_invocation_timeout_ms={}>",
            self.max_active_sandboxes,
            self.max_message_bytes,
            self.default_invocation_timeout_ms,
            self.max_invocation_timeout_ms
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
    m.add_class::<PyEngineLimits>()?;
    Ok(())
}
