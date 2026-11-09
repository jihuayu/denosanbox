//! Public data model, errors and raw backend contract for `denosanbox`.
//!
//! Nothing in this crate exposes `deno_core` or V8 types. The `denosanbox`
//! facade re-exports these items in a stable language-binding-friendly shape.

mod backend;
mod capabilities;
mod config;
mod error;
mod fake;
mod host;
mod plugin;

pub use backend::{
    CreatedDriver, Driver, DriverControl, DriverFactory, Invocation, MetricsObserver, StartOptions,
    UnavailableFactory,
};
pub use capabilities::{Capabilities, Feature, PluginRequirements, RuntimeMetrics};
pub use config::{
    BackendKind, CloseReport, EngineConfig, EngineLimits, InvocationOptions, SandboxOptions,
    ShutdownReport,
};
pub use error::{Error, Result};
pub use fake::{FakeControl, FakeFactory};
pub use plugin::prepare_with_profile;
pub use plugin::{
    builtin_alias_target, is_reserved_builtin_path, prepare_with_profile, WORKER_THREADS_BUILTIN,
};
pub use plugin::{
    prepare, resolve_relative, scan_import_specifiers, validate_path, PluginSource, PreparedPlugin,
};

/// Version of the public facade data model.
pub const API_VERSION: &str = "0.1.0";

/// Offline host-side module and package preparation.
pub use denosanbox_modules as modules;

pub use denosanbox_policy as policy;
pub use denosanbox_policy::{
    HostRegistry, InvocationContext, Policy, RegisteredCallback, SandboxId,
};
pub use host::{HostContext, HostGrant};

mod host_audit;
mod host_schema;
pub use host::{CapabilitySchemas, HostConfig};
pub use host_audit::{AuditEvent, AuditOutcome, HostAudit};
pub use host_schema::{JsonSchema, SchemaError, SchemaLimits};

pub use denosanbox_mounts::MountAccess;
pub use host::{WorkerFuture, WorkerHandler};
