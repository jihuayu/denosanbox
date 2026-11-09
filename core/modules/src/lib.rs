//! `denosanbox-modules` — offline module preparation helpers for the Deno
//! in-process sandbox.
//!
//! This crate is deliberately small and network-free. It provides:
//!
//! * [`transpile_module`] — parse + transpile a single JS/TS module from an
//!   in-memory `(path, source)` pair.
//! * [`validate_path`] — reject module specifiers/paths that could be abused
//!   for path traversal or URL normalisation tricks.
//! * [`CompatibilityProfile`] — capability profile gate (no guest native,
//!   process, network or Node builtins in either profile).
//! * [`Package`] — host-prepared, integrity-checked, vendored pure-JS package
//!   graph, materialised into an immutable [`PreparedPackage`].
//!
//! Nothing here downloads files, installs npm packages or reads the network.

#![forbid(unsafe_code)]

mod error;
mod package;
mod path;
mod transpile;

pub use error::ModuleError;
pub use package::{
    content_digest, prepare_package, CompatibilityProfile, Limits, Package, PreparedPackage,
    BACKEND_VERSION, DEFAULT_LIMITS,
};
pub use path::validate_path;
pub use transpile::transpile_module;
