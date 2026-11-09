//! Trusted N-API addon. Guest code runs only through the Rust facade.
//!
//! The addon owns no policy, accounting or filesystem logic: it forwards
//! trusted host configuration to the public facade and reports facade results
//! unchanged. Guest JSON and guest identifiers never select authority.

use denosanbox::{
    modules::CompatibilityProfile, policy, BackendKind, CloseReport, Engine, EngineConfig,
    EngineLimits, HostGrant, InvocationOptions, MountAccess, PluginSource, Policy, PreparedPlugin,
    RegisteredCallback, SandboxHandle, SandboxOptions,
};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn native_error(error: denosanbox::Error) -> napi::Error {
    napi::Error::new(
        Status::GenericFailure,
        format!("{}: {}", error.kind(), error),
    )
}

fn invalid(message: String) -> napi::Error {
    napi::Error::new(
        Status::InvalidArg,
        format!("invalid_configuration: {message}"),
    )
}

/// Trusted host options written as ASCII JSON.
#[napi(object)]
pub struct NativeEngineOptions {
    #[napi(js_name = "hostCapabilities")]
    pub host_capabilities: Option<Vec<String>>,
    #[napi(js_name = "readRoots")]
    pub read_roots: Option<Vec<String>>,
    #[napi(js_name = "writeRoots")]
    pub write_roots: Option<Vec<String>>,
    #[napi(js_name = "heapTargetBytes")]
    pub heap_target_bytes: Option<i64>,
    #[napi(js_name = "memoryBudgetBytes")]
    pub memory_budget_bytes: Option<i64>,
    #[napi(js_name = "maxSandboxes")]
    pub max_sandboxes: Option<i64>,
    #[napi(js_name = "maxMessageBytes")]
    pub max_message_bytes: Option<i64>,
    #[napi(js_name = "deadlineMs")]
    pub deadline_ms: Option<i64>,
    #[napi(js_name = "queueDepth")]
    pub queue_depth: Option<i64>,
    #[napi(js_name = "maxLogBytes")]
    pub max_log_bytes: Option<i64>,
    // Engine hard limits (`EngineLimits`), enforced by the facade on every
    // create for this engine — including plugins prepared by another engine.
    #[napi(js_name = "maxActiveSandboxes")]
    pub max_active_sandboxes: Option<i64>,
    #[napi(js_name = "maxPendingInvocations")]
    pub max_pending_invocations: Option<i64>,
    /// Engine hard JSON message limit. Distinct from the policy
    /// `maxMessageBytes` above.
    #[napi(js_name = "engineMessageBytes")]
    pub engine_message_bytes: Option<i64>,
    #[napi(js_name = "defaultInvocationTimeoutMs")]
    pub default_invocation_timeout_ms: Option<i64>,
    #[napi(js_name = "maxInvocationTimeoutMs")]
    pub max_invocation_timeout_ms: Option<i64>,
    #[napi(js_name = "maxModules")]
    pub max_modules: Option<i64>,
    #[napi(js_name = "maxModuleBytes")]
    pub max_module_bytes: Option<i64>,
    #[napi(js_name = "maxGraphBytes")]
    pub max_graph_bytes: Option<i64>,
}

/// Trusted per-sandbox request, narrowed by the facade ceiling.
#[napi(object)]
pub struct NativeSandboxOptions {
    #[napi(js_name = "defaultTimeoutMs")]
    pub default_timeout_ms: Option<i64>,
    #[napi(js_name = "memoryLimitBytes")]
    pub memory_limit_bytes: Option<i64>,
    #[napi(js_name = "deadlineMs")]
    pub deadline_ms: Option<i64>,
    #[napi(js_name = "queueDepth")]
    pub queue_depth: Option<i64>,
    #[napi(js_name = "maxMessageBytes")]
    pub max_message_bytes: Option<i64>,
    #[napi(js_name = "maxLogBytes")]
    pub max_log_bytes: Option<i64>,
    pub capabilities: Option<Vec<String>>,
    #[napi(js_name = "readRoots")]
    pub read_roots: Option<Vec<String>>,
    #[napi(js_name = "writeRoots")]
    pub write_roots: Option<Vec<String>>,
    /// Task-local trusted callback scopes; the wrapper supplies them.
    pub scopes: Option<Vec<(String, String)>>,
}

#[napi(object)]
pub struct NativeCloseReport {
    #[napi(js_name = "closedCleanly")]
    pub closed_cleanly: bool,
    #[napi(js_name = "forcedTermination")]
    pub forced_termination: bool,
    #[napi(js_name = "invocationsCompleted")]
    pub invocations_completed: i64,
    #[napi(js_name = "invocationsCancelled")]
    pub invocations_cancelled: i64,
    #[napi(js_name = "pendingHostOperations")]
    pub pending_host_operations: i64,
}

impl From<CloseReport> for NativeCloseReport {
    fn from(report: CloseReport) -> Self {
        Self {
            closed_cleanly: report.closed_cleanly,
            forced_termination: report.forced_termination,
            invocations_completed: report.invocations_completed.min(i64::MAX as u64) as i64,
            invocations_cancelled: report.invocations_cancelled.min(i64::MAX as u64) as i64,
            pending_host_operations: report.pending_host_operations.min(i64::MAX as usize) as i64,
        }
    }
}

/// Read-only engine hard limits reported to the wrapper.
#[napi(object)]
pub struct NativeEngineLimits {
    #[napi(js_name = "maxActiveSandboxes")]
    pub max_active_sandboxes: i64,
    #[napi(js_name = "maxPendingInvocations")]
    pub max_pending_invocations: i64,
    #[napi(js_name = "maxMessageBytes")]
    pub max_message_bytes: i64,
    #[napi(js_name = "defaultInvocationTimeoutMs")]
    pub default_invocation_timeout_ms: i64,
    #[napi(js_name = "maxInvocationTimeoutMs")]
    pub max_invocation_timeout_ms: i64,
    #[napi(js_name = "maxModules")]
    pub max_modules: i64,
    #[napi(js_name = "maxModuleBytes")]
    pub max_module_bytes: i64,
    #[napi(js_name = "maxGraphBytes")]
    pub max_graph_bytes: i64,
}

impl From<&EngineLimits> for NativeEngineLimits {
    fn from(limits: &EngineLimits) -> Self {
        let n = |v: usize| v.min(i64::MAX as usize) as i64;
        Self {
            max_active_sandboxes: n(limits.max_active_sandboxes),
            max_pending_invocations: n(limits.max_pending_invocations),
            max_message_bytes: n(limits.max_message_bytes),
            default_invocation_timeout_ms: limits.default_invocation_timeout.as_millis() as i64,
            max_invocation_timeout_ms: limits.max_invocation_timeout.as_millis() as i64,
            max_modules: n(limits.max_modules),
            max_module_bytes: n(limits.max_module_bytes),
            max_graph_bytes: n(limits.max_graph_bytes),
        }
    }
}

/// Maps the caller's explicit profile. Missing means `javascript-only`;
/// anything else unknown is rejected rather than downgraded.
fn parse_profile(profile: Option<&str>) -> Result<CompatibilityProfile> {
    match profile {
        None | Some("javascript-only") => Ok(CompatibilityProfile::JavaScriptOnly),
        Some("pure-js-npm") => Ok(CompatibilityProfile::PureJsNpm),
        Some(other) => Err(invalid(format!(
            "unknown profile '{other}'; expected 'javascript-only' or 'pure-js-npm'"
        ))),
    }
}

fn positive(name: &str, value: Option<i64>) -> Result<Option<usize>> {
    match value {
        None => Ok(None),
        Some(0) => Err(invalid(format!("{name} must be greater than zero"))),
        Some(v) if v < 0 => Err(invalid(format!("{name} must be non-negative"))),
        Some(v) => Ok(Some(v as usize)),
    }
}

#[napi(js_name = "NativeEngine")]
pub struct NativeEngine {
    engine: Engine,
    callbacks: Mutex<BTreeMap<String, RegisteredCallback>>,
}

#[napi]
impl NativeEngine {
    #[napi(constructor)]
    pub fn new(options: Option<NativeEngineOptions>) -> Result<Self> {
        let options = options.unwrap_or(NativeEngineOptions {
            host_capabilities: None,
            read_roots: None,
            write_roots: None,
            heap_target_bytes: None,
            memory_budget_bytes: None,
            max_sandboxes: None,
            max_message_bytes: None,
            deadline_ms: None,
            queue_depth: None,
            max_log_bytes: None,
            max_active_sandboxes: None,
            max_pending_invocations: None,
            engine_message_bytes: None,
            default_invocation_timeout_ms: None,
            max_invocation_timeout_ms: None,
            max_modules: None,
            max_module_bytes: None,
            max_graph_bytes: None,
        });
        let mut engine_limits = EngineLimits::default();
        if let Some(v) = positive("maxActiveSandboxes", options.max_active_sandboxes)? {
            engine_limits.max_active_sandboxes = v;
        }
        if let Some(v) = positive("maxPendingInvocations", options.max_pending_invocations)? {
            engine_limits.max_pending_invocations = v;
        }
        if let Some(v) = positive("engineMaxMessageBytes", options.engine_message_bytes)? {
            engine_limits.max_message_bytes = v;
        }
        if let Some(ms) = options.default_invocation_timeout_ms {
            if ms <= 0 {
                return Err(invalid(
                    "defaultInvocationTimeoutMs must be greater than zero".into(),
                ));
            }
            engine_limits.default_invocation_timeout = Duration::from_millis(ms as u64);
        }
        if let Some(ms) = options.max_invocation_timeout_ms {
            if ms <= 0 {
                return Err(invalid(
                    "maxInvocationTimeoutMs must be greater than zero".into(),
                ));
            }
            engine_limits.max_invocation_timeout = Duration::from_millis(ms as u64);
        }
        if let Some(v) = positive("maxModules", options.max_modules)? {
            engine_limits.max_modules = v;
        }
        if let Some(v) = positive("maxModuleBytes", options.max_module_bytes)? {
            engine_limits.max_module_bytes = v;
        }
        if let Some(v) = positive("maxGraphBytes", options.max_graph_bytes)? {
            engine_limits.max_graph_bytes = v;
        }
        // The facade validates every hard limit on construction.
        engine_limits
            .validate()
            .map_err(|err| invalid(err.to_string()))?;
        let mut ceiling = Policy::default();
        for name in options.host_capabilities.unwrap_or_default() {
            ceiling = ceiling
                .allow_capability(&name)
                .map_err(|_| invalid(format!("invalid host capability name '{name}'")))?;
        }
        for root in options.read_roots.unwrap_or_default() {
            ceiling = ceiling
                .allow_read(&root)
                .map_err(|_| invalid(format!("invalid read root '{root}'")))?;
        }
        for root in options.write_roots.unwrap_or_default() {
            ceiling = ceiling
                .allow_write(&root)
                .map_err(|_| invalid(format!("invalid write root '{root}'")))?;
        }
        let mut limits = policy::Limits::default();
        if let Some(v) = positive("heapTargetBytes", options.heap_target_bytes)? {
            limits = limits.with_heap_target_bytes(v);
        }
        if let Some(v) = positive("memoryBudgetBytes", options.memory_budget_bytes)? {
            limits = limits.with_memory_budget_bytes(v);
        }
        if let Some(v) = positive("maxSandboxes", options.max_sandboxes)? {
            limits = limits.with_max_sandboxes(v);
        }
        if let Some(v) = positive("maxMessageBytes", options.max_message_bytes)? {
            limits = limits.with_max_message_bytes(v);
        }
        if let Some(v) = options.deadline_ms {
            if v <= 0 {
                return Err(invalid("deadlineMs must be greater than zero".into()));
            }
            limits = limits.with_deadline(Duration::from_millis(v as u64));
        }
        if let Some(v) = positive("queueDepth", options.queue_depth)? {
            limits = limits.with_queue_depth(v);
        }
        if let Some(v) = positive("maxLogBytes", options.max_log_bytes)? {
            limits = limits.with_max_log_bytes(v);
        }
        // The engine ceiling is trusted host configuration: it may grant the
        // explicit capabilities/roots above. Per-sandbox requests are still
        // narrowed against it by `Engine::with_policy`/`create`.
        let ceiling = ceiling.with_limits(limits);
        Ok(Self {
            engine: Engine::with_policy(
                EngineConfig::new(BackendKind::Deno).with_limits(engine_limits),
                ceiling,
            )
            .map_err(native_error)?,
            callbacks: Mutex::new(BTreeMap::new()),
        })
    }

    #[napi]
    pub fn register_host(
        &self,
        name: String,
        callback: Function<'_, FnArgs<(String, String)>, Promise<String>>,
        input_limit: u32,
        output_limit: u32,
    ) -> Result<()> {
        let dispatch = Arc::new(
            callback
                .build_threadsafe_function::<FnArgs<(String, String)>>()
                .callee_handled::<false>()
                .weak::<true>()
                .max_queue_size::<32>()
                .build()?,
        );
        let registered=self.engine.register_host_async(&name,input_limit as usize,output_limit as usize,move |context,input| {
            let dispatch=dispatch.clone();
            async move {
                context.check_live()?;
                let metadata=serde_json::json!({"sandboxId":context.sandbox_id().to_string(),"scope":context.scope(),"remainingMs":context.deadline().remaining().map(|d|d.as_millis() as u64)}).to_string();
                let input=String::from_utf8(input).map_err(|_|policy::Error::invalid_argument("invalid host JSON"))?;
                let promise=dispatch.call_async_catch(FnArgs::from((metadata,input))).await.map_err(|_|policy::Error::invalid_argument("Node callback dispatch failed"))?;
                let output=promise.await.map_err(|_|policy::Error::invalid_argument("Node callback failed"))?;
                context.check_live()?;
                if output.len()>output_limit as usize { return Err(policy::Error::limit_exceeded("Node callback output exceeds limit")); }
                Ok(output.into_bytes())
            }
        }).map_err(native_error)?;
        self.callbacks
            .lock()
            .map_err(|_| {
                napi::Error::new(
                    Status::GenericFailure,
                    "internal: callback registry unavailable",
                )
            })?
            .insert(name, registered);
        Ok(())
    }

    /// Freezes the graph and returns its shape to the wrapper; the returned
    /// path set only describes the immutable facade graph.
    #[napi]
    pub fn prepare(&self, source: String, entrypoint: String) -> Result<Vec<String>> {
        let plugin = self
            .engine
            .prepare(PluginSource::single(entrypoint, source))
            .map_err(native_error)?;
        plugin_paths(&plugin)
    }

    /// Freezes a multi-module graph under an explicit profile.
    #[napi]
    pub fn prepare_modules(
        &self,
        modules_json: String,
        entrypoint: String,
        profile: Option<String>,
    ) -> Result<Vec<String>> {
        if modules_json.len() > 32 * 1024 * 1024 {
            return Err(invalid(
                "modulesJSON exceeds the 32 MiB engine graph limit".into(),
            ));
        }
        let modules: BTreeMap<String, String> = serde_json::from_str(&modules_json)
            .map_err(|err| invalid(format!("modulesJSON must be an object of strings: {err}")))?;
        let profile = parse_profile(profile.as_deref())?;
        let source = PluginSource {
            entrypoint,
            modules: modules.into_iter().collect(),
        };
        let plugin = self
            .engine
            .prepare_with_profile(source, profile)
            .map_err(native_error)?;
        plugin_paths(&plugin)
    }

    #[napi]
    pub async fn create(
        &self,
        source: String,
        entrypoint: String,
        grants_json: String,
        options: Option<NativeSandboxOptions>,
    ) -> Result<NativeSandbox> {
        self.create_inner(
            PluginSource::single(entrypoint, source),
            CompatibilityProfile::JavaScriptOnly,
            grants_json,
            options,
        )
        .await
    }

    /// Creates from a frozen multi-module graph under the caller's explicit,
    /// validated profile. Unknown profile strings are rejected here — never
    /// silently downgraded to `javascript-only`.
    #[napi]
    pub async fn create_modules(
        &self,
        modules_json: String,
        entrypoint: String,
        profile: String,
        grants_json: String,
        options: Option<NativeSandboxOptions>,
    ) -> Result<NativeSandbox> {
        let profile = parse_profile(Some(&profile))?;
        if modules_json.len() > 32 * 1024 * 1024 {
            return Err(invalid(
                "modulesJSON exceeds the 32 MiB engine graph limit".into(),
            ));
        }
        let modules: BTreeMap<String, String> = serde_json::from_str(&modules_json)
            .map_err(|err| invalid(format!("modulesJSON must be an object of strings: {err}")))?;
        self.create_inner(
            PluginSource {
                entrypoint,
                modules: modules.into_iter().collect(),
            },
            profile,
            grants_json,
            options,
        )
        .await
    }

    #[napi]
    pub async fn shutdown(&self) -> Result<bool> {
        Ok(self.engine.shutdown().await.map_err(native_error)?.clean)
    }

    #[napi(getter)]
    pub fn active_sandboxes(&self) -> u32 {
        self.engine.active_sandboxes().min(u32::MAX as usize) as u32
    }

    /// Engine hard limits as configured (the same values the facade enforces).
    #[napi(getter)]
    pub fn limits(&self) -> NativeEngineLimits {
        NativeEngineLimits::from(self.engine.limits())
    }

    #[napi]
    pub fn mount_directory(
        &self,
        sandbox: &NativeSandbox,
        prefix: String,
        path: String,
        access: String,
    ) -> Result<()> {
        let access = match access.as_str() {
            "read" => MountAccess::ReadOnly,
            "read-write" => MountAccess::ReadWrite,
            other => {
                return Err(invalid(format!(
                    "unknown mount access '{other}'; expected 'read' or 'read-write'"
                )))
            }
        };
        sandbox
            .sandbox
            .mount_directory(&prefix, std::path::Path::new(&path), access)
            .map_err(native_error)
    }
}

impl NativeEngine {
    async fn create_inner(
        &self,
        source: PluginSource,
        profile: CompatibilityProfile,
        grants_json: String,
        options: Option<NativeSandboxOptions>,
    ) -> Result<NativeSandbox> {
        let plugin = self
            .engine
            .prepare_with_profile(source, profile)
            .map_err(native_error)?;
        if grants_json.len() > 1024 * 1024 {
            return Err(invalid("host grants too large".into()));
        }
        let requested_grants: BTreeMap<String, String> = serde_json::from_str(&grants_json)
            .map_err(|_| invalid("invalid host grants".into()))?;
        let options = options.unwrap_or(NativeSandboxOptions {
            default_timeout_ms: None,
            memory_limit_bytes: None,
            deadline_ms: None,
            queue_depth: None,
            max_message_bytes: None,
            max_log_bytes: None,
            capabilities: None,
            read_roots: None,
            write_roots: None,
            scopes: None,
        });
        let mut requested = Policy::default();
        for name in options.capabilities.unwrap_or_default() {
            requested = requested
                .allow_capability(&name)
                .map_err(|_| invalid(format!("invalid capability name '{name}'")))?;
        }
        for root in options.read_roots.unwrap_or_default() {
            requested = requested
                .allow_read(&root)
                .map_err(|_| invalid(format!("invalid read root '{root}'")))?;
        }
        for root in options.write_roots.unwrap_or_default() {
            requested = requested
                .allow_write(&root)
                .map_err(|_| invalid(format!("invalid write root '{root}'")))?;
        }
        let mut policy_limits = requested.limits();
        if let Some(ms) = options.deadline_ms {
            if ms <= 0 {
                return Err(invalid("deadlineMs must be greater than zero".into()));
            }
            policy_limits = policy_limits.with_deadline(Duration::from_millis(ms as u64));
        }
        if let Some(v) = positive("queueDepth", options.queue_depth)? {
            policy_limits = policy_limits.with_queue_depth(v);
        }
        if let Some(v) = positive("maxMessageBytes", options.max_message_bytes)? {
            policy_limits = policy_limits.with_max_message_bytes(v);
        }
        if let Some(v) = positive("maxLogBytes", options.max_log_bytes)? {
            policy_limits = policy_limits.with_max_log_bytes(v);
        }
        requested = requested.with_limits(policy_limits);

        // Scopes come from the wrapper's task-local grant table, never from
        // guest JSON. Every requested grant must be known there.
        let scopes: BTreeMap<String, String> =
            options.scopes.unwrap_or_default().into_iter().collect();
        let mut grants = Vec::new();
        {
            let registered = self.callbacks.lock().map_err(|_| {
                napi::Error::new(
                    Status::GenericFailure,
                    "internal: callback registry unavailable",
                )
            })?;
            for (name, _guest_ignored_scope) in requested_grants {
                let callback = registered
                    .get(&name)
                    .ok_or_else(|| invalid(format!("unknown host callback '{name}'")))?;
                let trusted_scope = scopes.get(&name).ok_or_else(|| {
                    invalid(format!("host grant for '{name}' has no trusted scope"))
                })?;
                requested = requested
                    .allow_capability(&name)
                    .map_err(|_| invalid(format!("invalid capability name '{name}'")))?;
                grants.push(HostGrant {
                    callback: callback.clone(),
                    scope: trusted_scope.clone(),
                });
            }
        }
        let sandbox = self
            .engine
            .create(
                plugin,
                SandboxOptions {
                    default_invocation_timeout: options
                        .default_timeout_ms
                        .map(|ms| Duration::from_millis(ms.max(0) as u64)),
                    memory_limit_bytes: positive("memoryLimitBytes", options.memory_limit_bytes)?,
                    policy: requested,
                    host_grants: grants,
                    ..SandboxOptions::default()
                },
            )
            .await
            .map_err(native_error)?;
        Ok(NativeSandbox { sandbox })
    }
}

fn plugin_paths(plugin: &PreparedPlugin) -> Result<Vec<String>> {
    let paths: BTreeSet<String> = plugin
        .iter_modules()
        .map(|(path, _)| path.to_string())
        .collect();
    Ok(paths.into_iter().collect())
}

#[napi(js_name = "NativeSandbox")]
pub struct NativeSandbox {
    sandbox: SandboxHandle,
}

#[napi]
impl NativeSandbox {
    #[napi(getter)]
    pub fn id(&self) -> String {
        self.sandbox.id().to_string()
    }

    #[napi]
    pub async fn call(
        &self,
        export: String,
        input_json: String,
        timeout_ms: Option<i64>,
    ) -> Result<String> {
        // Bound before parsing using the default core 1 MiB limit.
        if input_json.len() > 1024 * 1024 {
            return Err(invalid("JSON input exceeds 1 MiB".into()));
        }
        let input = serde_json::from_str(&input_json)
            .map_err(|_| invalid("malformed JSON input".into()))?;
        let timeout = match timeout_ms {
            None => None,
            Some(ms) if ms <= 0 => {
                return Err(napi::Error::new(
                    Status::InvalidArg,
                    "invalid_configuration: timeout must be at least 1ms",
                ))
            }
            Some(ms) => Some(Duration::from_millis(ms as u64)),
        };
        let options = InvocationOptions {
            timeout,
            deadline: None,
        };
        let value = self
            .sandbox
            .call(&export, input, options)
            .await
            .map_err(native_error)?;
        serde_json::to_string(&value).map_err(|_| {
            napi::Error::new(
                Status::GenericFailure,
                "serialization: result cannot be encoded",
            )
        })
    }

    #[napi]
    pub fn cancel(&self) {
        self.sandbox.cancel();
    }

    #[napi]
    pub async fn close(&self) -> Result<NativeCloseReport> {
        Ok(self.sandbox.close().await.map_err(native_error)?.into())
    }
}
