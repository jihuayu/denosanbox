//! Deny-default, resource-aware policy with narrowing-only composition.

use std::collections::BTreeSet;

use crate::error::{Error, Result};
use crate::limits::Limits;
use crate::net::NetworkRules;
use crate::path::VirtualPath;

/// Alias kept for runtime ergonomics.
pub type PolicyError = Error;

fn check_cap_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.:-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Error::invalid_argument("invalid capability name"))
    }
}

fn covered(set: &BTreeSet<VirtualPath>, p: &VirtualPath) -> bool {
    set.iter().any(|g| g.is_ancestor_or_equal(p))
}

fn intersect_subtrees(
    a: &BTreeSet<VirtualPath>,
    b: &BTreeSet<VirtualPath>,
) -> BTreeSet<VirtualPath> {
    let mut out = BTreeSet::new();
    for x in a {
        for y in b {
            if x.is_ancestor_or_equal(y) {
                out.insert(y.clone());
            } else if y.is_ancestor_or_equal(x) {
                out.insert(x.clone());
            }
        }
    }
    out
}

/// Effective permissions for one sandbox. `Policy::default()` denies all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    read: BTreeSet<VirtualPath>,
    write: BTreeSet<VirtualPath>,
    caps: BTreeSet<String>,
    network: NetworkRules,
    limits: Limits,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            read: BTreeSet::new(),
            write: BTreeSet::new(),
            caps: BTreeSet::new(),
            network: NetworkRules::deny_all(),
            limits: Limits::default(),
        }
    }
}

impl Policy {
    /// Builder: grant read access to a virtual subtree.
    pub fn allow_read(mut self, path: &str) -> Result<Self> {
        self.read.insert(VirtualPath::parse(path)?);
        Ok(self)
    }
    /// Builder: grant write access to a virtual subtree.
    pub fn allow_write(mut self, path: &str) -> Result<Self> {
        self.write.insert(VirtualPath::parse(path)?);
        Ok(self)
    }
    /// Builder: grant a host capability.
    pub fn allow_capability(mut self, name: &str) -> Result<Self> {
        check_cap_name(name)?;
        self.caps.insert(name.to_string());
        Ok(self)
    }
    /// Builder: replace network rules (use [`NetworkRules`] builders).
    #[must_use]
    pub fn with_network(mut self, network: NetworkRules) -> Self {
        self.network = network;
        self
    }
    /// Builder: replace limits with stricter ones (never widens silently).
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = self.limits.narrow(limits);
        self
    }

    /// Readable subtrees.
    #[must_use]
    pub fn read_roots(&self) -> &BTreeSet<VirtualPath> {
        &self.read
    }
    /// Writable subtrees.
    #[must_use]
    pub fn write_roots(&self) -> &BTreeSet<VirtualPath> {
        &self.write
    }
    /// Granted capabilities.
    pub fn capabilities(&self) -> impl Iterator<Item = &str> {
        self.caps.iter().map(String::as_str)
    }
    /// Network rules.
    #[must_use]
    pub fn network(&self) -> &NetworkRules {
        &self.network
    }
    /// Limits.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Check a read of an already-parsed virtual path.
    pub fn check_read(&self, p: &VirtualPath) -> Result<()> {
        if covered(&self.read, p) {
            Ok(())
        } else {
            Err(Error::denied("read denied"))
        }
    }
    /// Check a write of an already-parsed virtual path.
    pub fn check_write(&self, p: &VirtualPath) -> Result<()> {
        if covered(&self.write, p) {
            Ok(())
        } else {
            Err(Error::denied("write denied"))
        }
    }
    /// Parse then check read.
    pub fn check_read_str(&self, p: &str) -> Result<()> {
        self.check_read(&VirtualPath::parse(p)?)
    }
    /// Parse then check write.
    pub fn check_write_str(&self, p: &str) -> Result<()> {
        self.check_write(&VirtualPath::parse(p)?)
    }
    /// Check a host capability grant.
    pub fn check_capability(&self, name: &str) -> Result<()> {
        check_cap_name(name)?;
        if self.caps.contains(name) {
            Ok(())
        } else {
            Err(Error::denied("capability denied"))
        }
    }

    /// True when every grant here is covered by `other`.
    #[must_use]
    pub fn is_subset_of(&self, other: &Policy) -> bool {
        self.read.iter().all(|p| covered(&other.read, p))
            && self.write.iter().all(|p| covered(&other.write, p))
            && self.caps.is_subset(&other.caps)
            && self.network.is_subset_of(&other.network)
            && self.limits.narrow(other.limits) == self.limits
    }

    /// Intersection: only permissions present in both. Always narrowing.
    #[must_use]
    pub fn intersect(&self, other: &Policy) -> Policy {
        Policy {
            read: intersect_subtrees(&self.read, &other.read),
            write: intersect_subtrees(&self.write, &other.write),
            caps: self.caps.intersection(&other.caps).cloned().collect(),
            network: self.network.intersection(&other.network),
            limits: self.limits.narrow(other.limits),
        }
    }

    /// Self-narrow to `requested`; fails if `requested` is not already within
    /// this policy (so a caller can never widen its own rights).
    pub fn narrow(&self, requested: &Policy) -> Result<Policy> {
        if !requested.is_subset_of(self) {
            return Err(Error::denied("narrowing request exceeds current policy"));
        }
        Ok(self.intersect(requested))
    }

    /// Derive a child policy.
    ///
    /// * `host` — host root ceiling.
    /// * `parent` — parent's effective policy.
    /// * `delegation` — the part of `parent` the parent is allowed to hand out;
    ///   must be a subset of `parent`.
    /// * `requested` — what the child asks for; clamped, never widened.
    ///
    /// Result = `requested ∩ delegation ∩ host ∩ parent`.
    pub fn child(
        host: &Policy,
        parent: &Policy,
        delegation: &Policy,
        requested: &Policy,
    ) -> Result<Policy> {
        if !parent.is_subset_of(host) {
            return Err(Error::denied("parent policy exceeds host policy"));
        }
        if !delegation.is_subset_of(parent) {
            return Err(Error::denied("delegation exceeds parent policy"));
        }
        Ok(requested
            .intersect(delegation)
            .intersect(parent)
            .intersect(host))
    }

    /// Explicitly unsupported: host filesystem mounts.
    pub fn mount_host_path(&self, _host_path: &str) -> Result<VirtualPath> {
        Err(Error::unsupported(
            "host mounts are unsupported until race-safe",
        ))
    }
    /// Explicitly unsupported: symlinks inside the virtual FS.
    pub fn create_symlink(&self, _target: &str, _link: &str) -> Result<()> {
        Err(Error::unsupported(
            "symlinks are unsupported until race-safe",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    fn basel() -> Policy {
        Policy::default()
            .allow_read("/app")
            .unwrap()
            .allow_write("/tmp/s")
            .unwrap()
            .allow_capability("clock")
            .unwrap()
    }

    #[test]
    fn default_denies_everything() {
        let p = Policy::default();
        assert_eq!(
            p.check_read_str("/app/x").unwrap_err().kind(),
            ErrorKind::Denied
        );
        assert_eq!(
            p.check_capability("clock").unwrap_err().kind(),
            ErrorKind::Denied
        );
        assert!(p.capabilities().next().is_none());
    }

    #[test]
    fn subtree_checks_not_string_prefix() {
        let p = basel();
        p.check_read_str("/app/a/b").unwrap();
        assert!(p.check_read_str("/application").is_err());
        assert!(p.check_write_str("/app/x").is_err());
        assert!(p.check_read_str("/app/../etc").is_err());
    }

    #[test]
    fn child_intersects_all_four_layers() {
        let host = basel().allow_capability("random").unwrap();
        let parent = host
            .clone()
            .allow_read("/app/data")
            .unwrap()
            .allow_capability("random")
            .unwrap();
        let delegation = Policy::default()
            .allow_read("/app/data/sub")
            .unwrap()
            .allow_capability("random")
            .unwrap();
        let requested = Policy::default()
            .allow_read("/app")
            .unwrap()
            .allow_write("/tmp/s")
            .unwrap()
            .allow_capability("random")
            .unwrap()
            .allow_capability("fs.host")
            .unwrap();
        let child = Policy::child(&host, &parent, &delegation, &requested).unwrap();
        assert!(child.check_read_str("/app/data/sub/x").is_ok());
        assert!(child.check_read_str("/app/data/other").is_err());
        assert!(child.check_capability("random").is_ok());
        assert!(child.check_capability("fs.host").is_err());
        assert!(child.check_write_str("/tmp/s/a").is_err());
    }

    #[test]
    fn bad_delegation_and_widening_rejected() {
        let host = basel();
        let parent = Policy::default();
        let wide = basel().allow_read("/etc").unwrap();
        assert!(Policy::child(&host, &parent, &wide, &wide).is_err());
        assert!(Policy::child(&host, &host, &host, &wide).is_ok());
        assert_eq!(host.narrow(&wide).unwrap_err().kind(), ErrorKind::Denied);
        assert!(host.narrow(&host).is_ok());
    }

    #[test]
    fn bad_capability_names_rejected() {
        assert!(Policy::default().allow_capability("Bad Name").is_err());
        assert!(Policy::default().allow_capability("").is_err());
    }
}
