//! Public data model, errors and raw backend contract for `denosanbox`.
//!
//! Nothing in this crate exposes `deno_core` or V8 types. The `denosanbox`
//! facade re-exports these items in a stable language-binding-friendly shape.

mod backend;
mod config;
mod error;
mod fake;
mod plugin;

pub use backend::{
    CreatedDriver, Driver, DriverControl, DriverFactory, Invocation, UnavailableFactory,
};
pub use config::{
    BackendKind, CloseReport, EngineConfig, EngineLimits, InvocationOptions, SandboxOptions,
    ShutdownReport,
};
pub use error::{Error, Result};
pub use fake::{FakeControl, FakeFactory};
pub use plugin::{
    prepare, resolve_relative, scan_import_specifiers, validate_path, PluginSource, PreparedPlugin,
};

/// Version of the public facade data model.
pub const API_VERSION: &str = "0.1.0";
