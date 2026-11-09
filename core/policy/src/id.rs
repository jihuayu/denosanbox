//! Trusted identity types.
//!
//! [`SandboxId`] has no deserialization path on purpose: identities are minted
//! only by the host, via [`SandboxId::mint`], and can never be constructed from
//! guest-supplied bytes. There is deliberately no `FromStr`, no
//! `serde::Deserialize`, and no public constructor taking an arbitrary value.
//! Guest data that must reference a sandbox travels as an *untrusted* tag that
//! the host maps to its own [`SandboxId`].

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic generation counter used to invalidate handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Generation(pub u64);

impl Generation {
    /// The generation before any resource was created.
    pub const ZERO: Generation = Generation(0);

    /// Next generation, saturating (never wrapping to a live-looking value).
    #[must_use]
    pub fn next(self) -> Generation {
        Generation(self.0.saturating_add(1))
    }

    /// Raw value, for host-side logging only.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gen{}", self.0)
    }
}

static NEXT_SANDBOX: AtomicU64 = AtomicU64::new(1);

/// A trusted, host-minted sandbox identity.
///
/// The payload is an opaque, non-forgeable-looking 128-bit value assembled from
/// a process-unique counter and a per-mint clock component. It is **not** a
/// secret and is not a capability: authorization always goes through policy and
/// handle ownership checks, never through the id alone.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SandboxId {
    hi: u64,
    lo: u64,
}

impl SandboxId {
    /// Mint a fresh identity. Host-only by convention: call this from the host
    /// runtime, never with guest-controlled data.
    #[must_use]
    pub fn mint() -> SandboxId {
        let n = NEXT_SANDBOX.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        SandboxId {
            hi: nanos.rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15,
            lo: n,
        }
    }

    /// Deterministic constructor for host tests. Not for guest use.
    #[must_use]
    pub fn from_host_parts(hi: u64, lo: u64) -> SandboxId {
        SandboxId { hi, lo }
    }

    /// Opaque sortable key, for host-side indexing.
    #[must_use]
    pub fn key(&self) -> (u64, u64) {
        (self.hi, self.lo)
    }
}

impl fmt::Debug for SandboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug output of an identity is intentionally redacted-ish but stable
        // enough for host logs; it is not a secret.
        write!(f, "SandboxId({:016x}{:016x})", self.hi, self.lo)
    }
}

impl fmt::Display for SandboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sbx-{:016x}{:016x}", self.hi, self.lo)
    }
}

/// The kind of kernel resource a [`ResourceHandle`] names.
///
/// Symlinks and host mounts are not representable: they are unsupported until a
/// race-safe implementation exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceKind {
    /// An in-memory file.
    File,
    /// An in-memory directory.
    Directory,
    /// A registered host callback handle.
    Callback,
    /// A network socket binding grant.
    Socket,
}

/// An owner- and generation-qualified handle to a resource.
///
/// Handles are values the host hands to an invocation; possession is not
/// authorization. Every use is validated against a [`crate::ResourceRegistry`]
/// which checks owner identity *and* the live generation, so a handle obtained
/// from an earlier generation can never touch a newer resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceHandle {
    owner: SandboxId,
    kind: ResourceKind,
    index: u32,
    generation: Generation,
}

impl ResourceHandle {
    /// Host-only construction; registries mint these.
    #[must_use]
    pub fn new(owner: SandboxId, kind: ResourceKind, index: u32, generation: Generation) -> Self {
        Self {
            owner,
            kind,
            index,
            generation,
        }
    }

    /// Owning sandbox.
    #[must_use]
    pub fn owner(&self) -> SandboxId {
        self.owner
    }

    /// Resource kind.
    #[must_use]
    pub fn kind(&self) -> ResourceKind {
        self.kind
    }

    /// Registry slot index.
    #[must_use]
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Generation the handle was minted for.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Whether this handle is owned by `id`.
    #[must_use]
    pub fn is_owned_by(&self, id: SandboxId) -> bool {
        self.owner == id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_is_unique_and_nonzero() {
        let a = SandboxId::mint();
        let b = SandboxId::mint();
        assert_ne!(a, b);
        assert_ne!(a.key(), (0, 0));
    }

    #[test]
    fn handle_carries_owner_and_generation() {
        let owner = SandboxId::from_host_parts(1, 2);
        let h = ResourceHandle::new(owner, ResourceKind::File, 7, Generation(3));
        assert!(h.is_owned_by(owner));
        assert!(!h.is_owned_by(SandboxId::from_host_parts(1, 3)));
        assert_eq!(h.generation(), Generation(3));
        assert_eq!(h.kind(), ResourceKind::File);
    }

    #[test]
    fn generation_next_saturates() {
        assert_eq!(Generation(u64::MAX).next(), Generation(u64::MAX));
        assert_eq!(Generation(0).next(), Generation(1));
    }
}
