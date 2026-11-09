//! Owner/generation-qualified resource handles.

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::id::{Generation, ResourceHandle, ResourceKind, SandboxId};
use crate::path::VirtualPath;

/// What a handle names; never a host object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceSpec {
    /// A file inside a sandbox's virtual FS.
    File {
        /// Virtual path.
        path: VirtualPath,
        /// Whether writes were requested.
        writable: bool,
    },
    /// A virtual directory.
    Directory {
        /// Virtual path.
        path: VirtualPath,
    },
    /// A registered host callback.
    Callback {
        /// Host-side callback id.
        callback_id: u64,
    },
    /// A validated network binding grant.
    Socket {
        /// Granted host.
        host: String,
        /// Granted port.
        port: u16,
    },
}

impl ResourceSpec {
    /// The kind tag for this spec.
    #[must_use]
    pub fn kind(&self) -> ResourceKind {
        match self {
            ResourceSpec::File { .. } => ResourceKind::File,
            ResourceSpec::Directory { .. } => ResourceKind::Directory,
            ResourceSpec::Callback { .. } => ResourceKind::Callback,
            ResourceSpec::Socket { .. } => ResourceKind::Socket,
        }
    }
}

/// A live registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRecord {
    owner: SandboxId,
    kind: ResourceKind,
    generation: Generation,
    spec: ResourceSpec,
}

impl ResourceRecord {
    /// Owning sandbox.
    #[must_use]
    pub fn owner(&self) -> SandboxId {
        self.owner
    }
    /// Kind tag.
    #[must_use]
    pub fn kind(&self) -> ResourceKind {
        self.kind
    }
    /// Generation at creation.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }
    /// Underlying spec.
    #[must_use]
    pub fn spec(&self) -> &ResourceSpec {
        &self.spec
    }
}

/// Handle table. Closing a sandbox bumps its generation, invalidating every
/// outstanding handle at once (defence against use-after-close races).
#[derive(Debug)]
pub struct ResourceRegistry {
    slots: HashMap<u32, ResourceRecord>,
    generations: HashMap<SandboxId, Generation>,
    next_index: u32,
    capacity: usize,
}

impl Default for ResourceRegistry {
    fn default() -> Self {
        Self::new(4096)
    }
}

impl ResourceRegistry {
    /// Registry with a hard entry cap.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            slots: HashMap::new(),
            generations: HashMap::new(),
            next_index: 1,
            capacity,
        }
    }

    /// Number of live handles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }
    /// Whether empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
    /// Current generation for an owner (zero if never used).
    #[must_use]
    pub fn generation(&self, owner: SandboxId) -> Generation {
        self.generations
            .get(&owner)
            .copied()
            .unwrap_or(Generation::ZERO)
    }

    /// Mint a handle for `owner`. Slots are never reused.
    pub fn insert(&mut self, owner: SandboxId, spec: ResourceSpec) -> Result<ResourceHandle> {
        if self.slots.len() >= self.capacity {
            return Err(Error::limit_exceeded("resource registry full"));
        }
        let generation = self
            .generations
            .entry(owner)
            .or_insert_with(|| Generation(1))
            .to_owned();
        let index = self.next_index;
        self.next_index = self
            .next_index
            .checked_add(1)
            .ok_or_else(|| Error::overflow("resource slot exhausted"))?;
        self.slots.insert(
            index,
            ResourceRecord {
                owner,
                kind: spec.kind(),
                generation,
                spec,
            },
        );
        Ok(ResourceHandle::new(
            owner,
            self.slots[&index].kind,
            index,
            generation,
        ))
    }

    /// Validate owner, generation, kind and liveness.
    pub fn get(&self, handle: ResourceHandle) -> Result<&ResourceRecord> {
        let rec = self
            .slots
            .get(&handle.index())
            .ok_or_else(|| Error::stale_handle("unknown resource handle"))?;
        if rec.owner != handle.owner()
            || rec.generation != handle.generation()
            || rec.kind != handle.kind()
        {
            return Err(Error::stale_handle("stale resource handle"));
        }
        Ok(rec)
    }

    /// Validate a guest handle against the trusted current caller identity.
    /// `get` is for trusted host inspection; guest operations must use this method.
    pub fn get_for(&self, caller: SandboxId, handle: ResourceHandle) -> Result<&ResourceRecord> {
        if handle.owner() != caller {
            return Err(Error::denied("resource belongs to another sandbox"));
        }
        self.get(handle)
    }

    /// Close a guest handle only when owned by the trusted caller.
    pub fn remove_for(
        &mut self,
        caller: SandboxId,
        handle: ResourceHandle,
    ) -> Result<ResourceRecord> {
        self.get_for(caller, handle)?;
        self.remove(handle)
    }

    /// Close one handle (same staleness checks as [`Self::get`]).
    pub fn remove(&mut self, handle: ResourceHandle) -> Result<ResourceRecord> {
        self.get(handle)?;
        self.slots
            .remove(&handle.index())
            .ok_or_else(|| Error::stale_handle("unknown resource handle"))
    }

    /// Close every handle for `owner` and invalidate its generation.
    pub fn close_all(&mut self, owner: SandboxId) -> usize {
        let before = self.slots.len();
        self.slots.retain(|_, r| r.owner != owner);
        self.generations
            .entry(owner)
            .and_modify(|g| *g = g.next())
            .or_insert_with(|| Generation(1));
        before - self.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> ResourceSpec {
        ResourceSpec::File {
            path: VirtualPath::parse(path).unwrap(),
            writable: false,
        }
    }

    #[test]
    fn validates_owner_and_generation() {
        let mut reg = ResourceRegistry::default();
        let a = SandboxId::from_host_parts(1, 1);
        let b = SandboxId::from_host_parts(1, 2);
        let h = reg.insert(a, file("/x")).unwrap();
        assert_eq!(reg.get(h).unwrap().owner(), a);
        let forged = ResourceHandle::new(b, h.kind(), h.index(), h.generation());
        assert!(reg.get(forged).is_err());
        let forged_gen = ResourceHandle::new(a, h.kind(), h.index(), Generation(9));
        assert!(reg.get(forged_gen).is_err());
    }

    #[test]
    fn close_all_bumps_generation_and_rejects_stale() {
        let mut reg = ResourceRegistry::default();
        let a = SandboxId::from_host_parts(2, 1);
        let h1 = reg.insert(a, file("/x")).unwrap();
        let gh1 = reg.insert(a, file("/y")).unwrap();
        assert_eq!(h1.generation(), gh1.generation());
        assert_eq!(reg.close_all(a), 2);
        assert!(reg.get(h1).is_err());
        assert!(reg.is_empty());
        let h2 = reg.insert(a, file("/z")).unwrap();
        assert!(h2.generation() > h1.generation());
        assert!(reg.get(h2).is_ok());
        assert!(reg.get(h1).is_err());
    }

    #[test]
    fn individual_close_and_capacity() {
        let mut reg = ResourceRegistry::new(1);
        let a = SandboxId::from_host_parts(3, 1);
        let h = reg.insert(a, file("/x")).unwrap();
        assert!(reg.insert(a, file("/y")).is_err());
        reg.remove(h).unwrap();
        assert!(reg.get(h).is_err());
        assert!(reg.insert(a, file("/y")).is_ok());
    }
}
