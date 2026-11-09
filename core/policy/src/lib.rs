//! Security primitives for an in-process JavaScript sandbox.
//!
//! Building blocks only — not a containment boundary by themselves. See
//! `README.md` for scope, integration contract and gaps.

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]

pub mod budget;
pub mod error;
pub mod id;
pub mod limits;
pub mod net;
pub mod path;
pub mod policy;
pub mod protocol;
pub mod registry;
pub mod resource;
pub mod vfs;

pub use budget::{Budget, CancellationToken, Deadline, Lease};
pub use error::{Error, ErrorKind, Result};
pub use id::{Generation, ResourceHandle, ResourceKind, SandboxId};
pub use limits::Limits;
pub use net::{requires_private_grant, HostPattern, NetworkRule, NetworkRules, PortRange};
pub use path::VirtualPath;
pub use policy::{Policy, PolicyError};
pub use protocol::{decode_error, encode_error, Frame, FrameKind, MAX_PAYLOAD, PROTOCOL_VERSION};
pub use registry::{
    AsyncCallback, Callback, CallbackFuture, CallbackId, Dispatch, HostRegistry, InvocationContext,
    InvocationRequest, RegisteredCallback, RetiredCallbacks,
};
pub use resource::{ResourceRecord, ResourceRegistry, ResourceSpec};
pub use vfs::{ResourceView, ViewLimits};

/// Crate version, reported by the protocol handshake.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
