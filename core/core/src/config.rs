//! Public configuration and report value objects.
//!
//! These types deliberately contain no `deno_core`, `v8` or backend-specific
//! values so language bindings can mirror them directly.

use std::time::{Duration, SystemTime};

use crate::Error;

/// Selects the runtime backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// Real `deno_core`/V8 backend.
    Deno,
    /// Deterministic, non-JavaScript backend used for lifecycle contract tests.
    ///
    /// It is never selected implicitly in production: [`EngineConfig::default`]
    /// uses [`BackendKind::Deno`].
    Fake,
}

impl BackendKind {
    /// Stable lowercase name for bindings.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deno => "deno",
            Self::Fake => "fake",
        }
    }
}

/// Engine-wide hard limits. Defaults are the documented foundation values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineLimits {
    /// Maximum simultaneously live sandboxes.
    pub max_active_sandboxes: usize,
    /// Maximum queued (not currently executing) invocations per sandbox.
    pub max_pending_invocations: usize,
    /// Maximum JSON input or output size in bytes.
    pub max_message_bytes: usize,
    /// Default invocation timeout when neither sandbox nor invocation overrides.
    pub default_invocation_timeout: Duration,
    /// Upper bound on any invocation timeout.
    pub max_invocation_timeout: Duration,
    /// Maximum number of modules in one prepared plugin.
    pub max_modules: usize,
    /// Maximum bytes for one module source.
    pub max_module_bytes: usize,
    /// Maximum bytes for the entire prepared graph.
    pub max_graph_bytes: usize,
}

impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            max_active_sandboxes: 4,
            max_pending_invocations: 32,
            max_message_bytes: 1024 * 1024,
            default_invocation_timeout: Duration::from_secs(30),
            max_invocation_timeout: Duration::from_secs(300),
            max_modules: 128,
            max_module_bytes: 8 * 1024 * 1024,
            max_graph_bytes: 32 * 1024 * 1024,
        }
    }
}

impl EngineLimits {
    /// Validates a limit set. All values must be non-zero and internally sane.
    pub fn validate(&self) -> Result<(), Error> {
        let fields = [
            ("max_active_sandboxes", self.max_active_sandboxes),
            ("max_pending_invocations", self.max_pending_invocations),
            ("max_message_bytes", self.max_message_bytes),
            ("max_modules", self.max_modules),
            ("max_module_bytes", self.max_module_bytes),
            ("max_graph_bytes", self.max_graph_bytes),
        ];
        for (name, value) in fields {
            if value == 0 {
                return Err(Error::InvalidConfiguration {
                    reason: format!("{name} must be greater than zero"),
                });
            }
        }
        if self.default_invocation_timeout.is_zero() {
            return Err(Error::InvalidConfiguration {
                reason: "default_invocation_timeout must be greater than zero".to_string(),
            });
        }
        if self.max_invocation_timeout.is_zero() {
            return Err(Error::InvalidConfiguration {
                reason: "max_invocation_timeout must be greater than zero".to_string(),
            });
        }
        if self.default_invocation_timeout > self.max_invocation_timeout {
            return Err(Error::InvalidConfiguration {
                reason: "default_invocation_timeout must not exceed max_invocation_timeout"
                    .to_string(),
            });
        }
        Ok(())
    }
}

/// Engine construction configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    /// Runtime backend to instantiate.
    pub backend: BackendKind,
    /// Hard engine limits.
    pub limits: EngineLimits,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            backend: BackendKind::Deno,
            limits: EngineLimits::default(),
        }
    }
}

impl EngineConfig {
    /// Builds a config selecting `backend` with default limits.
    pub fn new(backend: BackendKind) -> Self {
        Self {
            backend,
            limits: EngineLimits::default(),
        }
    }

    /// Replaces the limits after construction.
    pub fn with_limits(mut self, limits: EngineLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Validates the config. Called by `Engine::new`.
    pub fn validate(&self) -> Result<(), Error> {
        self.limits.validate()
    }
}

/// Opaque per-sandbox policy configuration.
///
/// Enforcement of capability grants belongs to the separate policy crate;
/// the foundation only carries a small, serializable bag of policy decisions
/// that the host may fill in later. The default denies every capability.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxOptions {
    /// Optional default timeout for calls on this sandbox.
    pub default_invocation_timeout: Option<Duration>,
    /// Optional memory limit hint forwarded to the backend.
    pub memory_limit_bytes: Option<usize>,
    /// Opaque host policy decisions; never interpreted as allow-all defaults.
    pub policy: std::collections::BTreeMap<String, String>,
}

impl SandboxOptions {
    /// Creates default deny-by-default options.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Per-call options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InvocationOptions {
    /// Relative timeout for this call.
    pub timeout: Option<Duration>,
    /// Absolute wall-clock deadline for this call.
    pub deadline: Option<SystemTime>,
}

impl InvocationOptions {
    /// A call with no explicit options, using sandbox/engine defaults.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Report returned by `SandboxHandle::close`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloseReport {
    /// Calls that completed successfully before close.
    pub invocations_completed: u64,
    /// Calls interrupted by cancellation or close.
    pub invocations_cancelled: u64,
    /// Whether the worker/isolate had to be terminated.
    pub forced_termination: bool,
    /// Whether the worker thread joined cleanly.
    pub closed_cleanly: bool,
}

/// Report returned by `Engine::shutdown`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Sandboxes that were closed by this shutdown.
    pub sandboxes_closed: usize,
    /// Sandboxes whose worker did not join within the shutdown grace period.
    pub sandboxes_forced: usize,
    /// True when every sandbox joined cleanly.
    pub clean: bool,
}
