//! `denosanbox` public facade.
//!
//! This crate is the only crate bindings should use. Its public surface
//! contains no `deno_core` or V8 types. The real Deno backend is selected by
//! default; the deterministic fake backend is explicit and intended for
//! contract tests.

mod engine;
mod worker_broker;

pub use denosanbox_core::{
    BackendKind, CloseReport, EngineConfig, EngineLimits, Error, InvocationOptions, PluginSource,
    PreparedPlugin, Result, SandboxOptions, ShutdownReport,
};
pub use engine::{Engine, SandboxHandle};

/// Truthful capability negotiation types.
pub use denosanbox_core::{Capabilities, Feature, PluginRequirements, RuntimeMetrics};

/// Public facade version, mirrored by `denosanbox-core::API_VERSION`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Offline host-side module and package preparation.
pub use denosanbox_core::modules;

pub use denosanbox_core::{
    policy, HostGrant, HostRegistry, InvocationContext, Policy, RegisteredCallback, SandboxId,
};

mod workers;
pub use workers::{WorkerHandle, WorkerOptions};

pub use denosanbox_core::{
    AuditEvent, AuditOutcome, CapabilitySchemas, HostAudit, HostConfig, JsonSchema, SchemaError,
    SchemaLimits,
};

pub use denosanbox_core::MountAccess;
