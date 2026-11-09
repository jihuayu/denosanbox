//! Sanitized, typed errors.
//!
//! Every error carries a stable [`ErrorKind`] and a human-readable message that
//! is safe to surface to untrusted guest code: it never embeds host paths,
//! environment values, or other host secrets. Host-side detail belongs in the
//! host's own logs, not in these messages.

use std::fmt;

/// Stable, machine-readable classification. The numeric codes are part of the
/// wire protocol and must not be renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum ErrorKind {
    /// Generic invalid argument supplied by the guest or host.
    InvalidArgument = 1,
    /// A virtual path was relative, contained traversal, or was malformed.
    InvalidPath = 2,
    /// An operation was not permitted by the effective policy.
    Denied = 3,
    /// A resource handle was unknown, stale, or owned by someone else.
    StaleHandle = 4,
    /// A resource limit (bytes, entries, …) would be exceeded.
    LimitExceeded = 5,
    /// An arithmetic or accounting overflow was detected.
    Overflow = 6,
    /// A deadline has passed.
    DeadlineExceeded = 7,
    /// The operation was cancelled by the host.
    Cancelled = 8,
    /// A protocol frame was malformed or used an unsupported version.
    Protocol = 9,
    /// The requested feature is intentionally unsupported.
    Unsupported = 10,
    /// A callback identifier is unknown or was revoked.
    UnknownCallback = 11,
    /// A callback produced a result for a stale generation.
    LateResult = 12,
    /// Network destination was not permitted.
    NetworkDenied = 13,
}

impl ErrorKind {
    /// Stable numeric code used by the wire protocol.
    #[must_use]
    pub const fn code(self) -> u16 {
        self as u16
    }

    /// Reconstruct a kind from a wire code, if it is a known code.
    #[must_use]
    pub const fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            1 => Self::InvalidArgument,
            2 => Self::InvalidPath,
            3 => Self::Denied,
            4 => Self::StaleHandle,
            5 => Self::LimitExceeded,
            6 => Self::Overflow,
            7 => Self::DeadlineExceeded,
            8 => Self::Cancelled,
            9 => Self::Protocol,
            10 => Self::Unsupported,
            11 => Self::UnknownCallback,
            12 => Self::LateResult,
            13 => Self::NetworkDenied,
            _ => return None,
        })
    }
}

/// Crate error type. `message` is always guest-safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    /// Construct an error from a kind and a guest-safe message.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The stable error class.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Guest-safe message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Whether the condition is transient and retrying could succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::DeadlineExceeded | ErrorKind::Cancelled | ErrorKind::Overflow
        )
    }
}

macro_rules! ctor {
    ($($name:ident => $kind:ident, $doc:literal;)*) => {
        impl Error {
            $(
                #[doc = $doc]
                #[must_use]
                pub fn $name(message: impl Into<String>) -> Self {
                    Self::new(ErrorKind::$kind, message)
                }
            )*
        }
    };
}

ctor! {
    invalid_argument => InvalidArgument, "Invalid argument.";
    invalid_path => InvalidPath, "Invalid virtual path.";
    denied => Denied, "Operation denied by policy.";
    stale_handle => StaleHandle, "Resource handle is stale or unknown.";
    limit_exceeded => LimitExceeded, "Resource limit exceeded.";
    overflow => Overflow, "Accounting overflow.";
    deadline_exceeded => DeadlineExceeded, "Deadline exceeded.";
    cancelled => Cancelled, "Operation cancelled.";
    protocol => Protocol, "Malformed or unsupported protocol frame.";
    unsupported => Unsupported, "Feature is intentionally unsupported.";
    unknown_callback => UnknownCallback, "Callback is unknown or revoked.";
    late_result => LateResult, "Result belongs to a stale generation.";
    network_denied => NetworkDenied, "Network destination denied.";
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;
