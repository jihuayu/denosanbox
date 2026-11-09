//! Thread-safe backend contract.
//!
//! A factory is thread-safe but builds a *non-Send* driver on the sandbox's
//! owning thread. The driver (and V8 isolate) never cross threads; only the
//! initialization closure and the separate cancellation control do.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::capabilities::{Capabilities, RuntimeMetrics};
use crate::config::{BackendKind, CloseReport, SandboxOptions};
use crate::plugin::PreparedPlugin;
use crate::{Error, Result};

/// One serialized invocation handed to a driver on its owning thread.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// Named ESM export to call.
    pub export: String,
    /// JSON input value.
    pub input: Value,
    /// Optional absolute deadline enforced by the backend.
    pub deadline: Option<SystemTime>,
    /// Effective relative timeout (deadline has precedence if earlier).
    pub timeout: Duration,
}

impl Invocation {
    /// Returns whether the absolute deadline has already elapsed.
    pub fn deadline_elapsed(&self) -> bool {
        self.deadline
            .map(|deadline| SystemTime::now() >= deadline)
            .unwrap_or(false)
    }
}

/// Independent, thread-safe cancellation control.
///
/// For the Deno backend this terminates the V8 isolate's current execution.
/// It is deliberately separate from the serialized call queue so cancellation
/// is not delayed behind pending calls.
pub trait DriverControl: Send + Sync {
    /// Requests termination of the current invocation and prevents new work.
    fn cancel(&self);
    /// Returns whether cancellation was requested.
    fn is_cancelled(&self) -> bool;

    /// Measured backend metrics for this runtime. Never blocks on the guest
    /// thread: implementations publish snapshots from the owning thread.
    fn metrics(&self) -> RuntimeMetrics {
        RuntimeMetrics::default()
    }
}

/// Measurement callback invoked on the owning worker thread after each
/// invocation. It must be cheap and must not call back into the driver.
pub type MetricsObserver = Arc<dyn Fn(&RuntimeMetrics) + Send + Sync>;

/// Trusted callback run by the backend on its own owner thread to report the
/// sandbox's spawn handshake. It returns the JSON value handed back to Rust.
pub type AckCallback = Arc<dyn Fn() -> std::result::Result<serde_json::Value, Error> + Send + Sync>;

/// Engine-provided hooks for [`Driver::start_with`].
#[derive(Clone, Default)]
pub struct StartOptions {
    /// Receives measured snapshots after every invocation.
    pub on_invocation: Option<MetricsObserver>,
    /// Resolves the sandbox's spawn acknowledgement on the owner thread.
    pub ack: Option<AckCallback>,
}

impl std::fmt::Debug for StartOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartOptions")
            .field("on_invocation", &self.on_invocation.is_some())
            .field("ack", &self.ack.is_some())
            .finish()
    }
}

/// A thread-affine runtime driver owned by a single worker thread.
///
/// This trait intentionally has no `Send`/`Sync` bound: implementations may
/// hold `deno_core::JsRuntime` and `Rc` values.
pub trait Driver {
    /// Initializes the runtime. Called exactly once on the owning worker thread
    /// before any invocation; startup errors are surfaced by `Engine::create`.
    fn start(&mut self) -> Result<()> {
        Ok(())
    }

    /// Initializes with engine hooks. The default stores the observer and then
    /// calls [`Driver::start`]; custom `invoke` implementations that want
    /// metrics should report them via [`Driver::metrics`] and call
    /// `StartOptions::observe`.
    fn start_with(&mut self, options: StartOptions) -> Result<()> {
        let _ = options;
        self.start()
    }

    /// Executes one invocation synchronously on the owning thread.
    fn invoke(&mut self, invocation: Invocation) -> Result<Value>;

    /// Releases the runtime and reports observed counters.
    fn close(self: Box<Self>) -> Result<CloseReport>;

    /// Measured runtime metrics. Unmeasured quantities stay `None`; a driver
    /// must never invent heap numbers it does not actually observe.
    fn metrics(&self) -> RuntimeMetrics {
        RuntimeMetrics::default()
    }
}

/// A driver factory result: an owned-thread initializer plus cancellation.
pub struct CreatedDriver {
    /// Runs on the worker thread and creates the non-Send driver.
    pub init: Box<dyn FnOnce() -> Result<Box<dyn Driver>> + Send>,
    /// Thread-safe cancellation handle usable before initialization.
    pub control: Arc<dyn DriverControl>,
}

/// Thread-safe factory. Implementations create fresh isolates/runtimes so
/// sandboxes never share global state or module instances.
pub trait DriverFactory: Send + Sync {
    /// Backend kind produced by this factory.
    fn kind(&self) -> BackendKind;

    /// Truthful baseline capabilities of this factory's backend.
    fn capabilities(&self) -> Capabilities {
        Capabilities::for_backend(self.kind())
    }

    /// Creates an initializer for one independent runtime.
    fn create(
        &self,
        plugin: PreparedPlugin,
        options: &SandboxOptions,
        context: Arc<crate::HostContext>,
    ) -> Result<CreatedDriver>;
}

/// A factory for a backend that was not compiled into this build.
pub struct UnavailableFactory {
    backend: BackendKind,
}

impl UnavailableFactory {
    /// Creates an unavailable factory for `backend`.
    pub fn new(backend: BackendKind) -> Self {
        Self { backend }
    }
}

impl DriverFactory for UnavailableFactory {
    fn kind(&self) -> BackendKind {
        self.backend
    }

    fn capabilities(&self) -> Capabilities {
        // Nothing is installed, so nothing may be claimed.
        Capabilities::for_backend(self.backend)
    }

    fn create(
        &self,
        _plugin: PreparedPlugin,
        _options: &SandboxOptions,
        _context: Arc<crate::HostContext>,
    ) -> Result<CreatedDriver> {
        Err(Error::RuntimeUnavailable {
            backend: self.backend.as_str().to_string(),
        })
    }
}
