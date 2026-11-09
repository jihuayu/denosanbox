//! Typed errors for the public denosanbox API.
//!
//! The enum is intentionally independent of `deno_core`/V8 so bindings can
//! expose it without leaking runtime-specific types.

use std::fmt;

/// Result alias used throughout the public API.
pub type Result<T> = std::result::Result<T, Error>;

/// Stable, classified errors returned by the facade and backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The configured backend was not compiled into this build.
    RuntimeUnavailable {
        /// Requested backend name.
        backend: String,
    },
    /// A backend or host capability is deliberately unsupported.
    UnsupportedFeature {
        /// Stable capability or feature name.
        feature: String,
    },
    /// TypeScript input was supplied without a supported transpilation path.
    UnsupportedLanguage {
        /// Human-readable reason.
        reason: String,
    },
    /// The plugin graph or one of its paths is invalid.
    InvalidPlugin {
        /// Human-readable reason.
        reason: String,
    },
    /// An imported module is not present in the immutable prepared graph.
    ModuleNotFound {
        /// Import specifier that could not be resolved.
        specifier: String,
        /// Module that contained the import, when known.
        referrer: Option<String>,
    },
    /// An import escaped the visible graph (scheme, bare specifier, parent
    /// traversal, query/fragment, unsupported module type, ...).
    ModuleDenied {
        /// Denied specifier.
        specifier: String,
        /// Human-readable reason.
        reason: String,
    },
    /// A configured limit was exceeded.
    LimitExceeded {
        /// Limit name.
        limit: String,
        /// Detail with actual and allowed values.
        detail: String,
    },
    /// The sandbox's serialized call queue is full.
    QueueFull {
        /// Configured pending-call capacity.
        capacity: usize,
    },
    /// The sandbox or engine has already been closed.
    Closed,
    /// An operation was denied by the trusted policy.
    Policy { kind: String, message: String },
    /// The invocation was cancelled by `SandboxHandle::cancel` or engine close.
    Cancelled,
    /// An absolute invocation deadline elapsed.
    DeadlineExceeded,
    /// A guest JavaScript error escaped module evaluation or the export call.
    Execution {
        /// Stable guest error rendering.
        message: String,
    },
    /// The backend hit an internal or runtime error.
    Backend {
        /// Backend detail.
        message: String,
    },
    /// Invalid API configuration.
    InvalidConfiguration {
        /// Human-readable reason.
        reason: String,
    },
    /// An internal invariant failed.
    Internal {
        /// Human-readable detail.
        message: String,
    },
}

impl Error {
    /// Stable machine-readable category, useful for bindings.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RuntimeUnavailable { .. } => "runtime_unavailable",
            Self::UnsupportedFeature { .. } => "unsupported_feature",
            Self::UnsupportedLanguage { .. } => "unsupported_language",
            Self::InvalidPlugin { .. } => "invalid_plugin",
            Self::ModuleNotFound { .. } => "module_not_found",
            Self::ModuleDenied { .. } => "module_denied",
            Self::LimitExceeded { .. } => "limit_exceeded",
            Self::QueueFull { .. } => "queue_full",
            Self::Closed => "closed",
            Self::Policy { .. } => "policy",
            Self::Cancelled => "cancelled",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Execution { .. } => "execution",
            Self::Backend { .. } => "backend",
            Self::InvalidConfiguration { .. } => "invalid_configuration",
            Self::Internal { .. } => "internal",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RuntimeUnavailable { backend } => {
                write!(
                    f,
                    "runtime backend '{backend}' is not available in this build"
                )
            }
            Self::UnsupportedFeature { feature } => {
                write!(f, "unsupported feature: {feature}")
            }
            Self::UnsupportedLanguage { reason } => {
                write!(f, "unsupported language: {reason}")
            }
            Self::InvalidPlugin { reason } => write!(f, "invalid plugin: {reason}"),
            Self::ModuleNotFound {
                specifier,
                referrer,
            } => match referrer {
                Some(referrer) => write!(
          f,
          "module '{specifier}' imported by '{referrer}' is not visible in the prepared graph"
        ),
                None => write!(
                    f,
                    "module '{specifier}' is not visible in the prepared graph"
                ),
            },
            Self::ModuleDenied { specifier, reason } => {
                write!(f, "denied module specifier '{specifier}': {reason}")
            }
            Self::LimitExceeded { limit, detail } => {
                write!(f, "limit '{limit}' exceeded: {detail}")
            }
            Self::QueueFull { capacity } => {
                write!(f, "sandbox call queue is full (capacity {capacity})")
            }
            Self::Closed => write!(f, "sandbox or engine is closed"),
            Self::Policy { kind, message } => write!(f, "{kind}: {message}"),
            Self::Cancelled => write!(f, "invocation cancelled"),
            Self::DeadlineExceeded => write!(f, "invocation deadline exceeded"),
            Self::Execution { message } => write!(f, "guest execution failed: {message}"),
            Self::Backend { message } => write!(f, "runtime backend error: {message}"),
            Self::InvalidConfiguration { reason } => {
                write!(f, "invalid configuration: {reason}")
            }
            Self::Internal { message } => write!(f, "internal error: {message}"),
        }
    }
}

impl std::error::Error for Error {}
