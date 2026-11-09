//! Thread-safe backend contract.
//!
//! A factory is thread-safe but builds a *non-Send* driver on the sandbox's
//! owning thread. The driver (and V8 isolate) never cross threads; only the
//! initialization closure and the separate cancellation control do.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::Value;

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

    /// Executes one invocation synchronously on the owning thread.
    fn invoke(&mut self, invocation: Invocation) -> Result<Value>;

    /// Releases the runtime and reports observed counters.
    fn close(self: Box<Self>) -> Result<CloseReport>;
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

    /// Creates an initializer for one independent runtime.
    fn create(&self, plugin: PreparedPlugin, options: &SandboxOptions) -> Result<CreatedDriver>;
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

    fn create(&self, _plugin: PreparedPlugin, _options: &SandboxOptions) -> Result<CreatedDriver> {
        Err(Error::RuntimeUnavailable {
            backend: self.backend.as_str().to_string(),
        })
    }
}
