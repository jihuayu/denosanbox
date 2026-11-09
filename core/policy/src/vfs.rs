//! In-memory private virtual file system, cwd, env and tmp.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};
use crate::id::SandboxId;
use crate::path::VirtualPath;
use crate::Policy;

/// Allocation bounds for a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewLimits {
    /// Maximum files.
    pub max_files: usize,
    /// Maximum directories.
    pub max_dirs: usize,
    /// Maximum bytes per file.
    pub max_file_bytes: usize,
    /// Maximum total bytes across all files.
    pub max_total_bytes: usize,
    /// Maximum logical env entries.
    pub max_env_entries: usize,
    /// Maximum env value bytes.
    pub max_env_value_bytes: usize,
}

impl Default for ViewLimits {
    fn default() -> Self {
        Self {
            max_files: 1024,
            max_dirs: 1024,
            max_file_bytes: 1024 * 1024,
            max_total_bytes: 16 * 1024 * 1024,
            max_env_entries: 64,
            max_env_value_bytes: 4096,
        }
    }
}

/// A sandbox-private virtual filesystem. Never touches the host FS, host
/// environment or processes; symlinks and host mounts are unsupported.
#[derive(Debug)]
pub struct ResourceView {
    id: SandboxId,
    policy: Policy,
    limits: ViewLimits,
    files: BTreeMap<VirtualPath, Vec<u8>>,
    dirs: BTreeSet<VirtualPath>,
    env: BTreeMap<String, String>,
    cwd: VirtualPath,
    total_bytes: usize,
    closed: bool,
}

impl ResourceView {
    /// Empty view rooted at `/` with a pre-created `/tmp`.
    #[must_use]
    pub fn new(id: SandboxId, policy: Policy, limits: ViewLimits) -> Self {
        let mut dirs = BTreeSet::new();
        dirs.insert(VirtualPath::root());
        dirs.insert(VirtualPath::parse("/tmp").expect("static path"));
        Self {
            id,
            policy,
            limits,
            files: BTreeMap::new(),
            dirs,
            env: BTreeMap::new(),
            cwd: VirtualPath::root(),
            total_bytes: 0,
            closed: false,
        }
    }

    /// Owning identity.
    #[must_use]
    pub fn sandbox_id(&self) -> SandboxId {
        self.id
    }
    /// Narrow future operation checks. Already-open host resources are not
    /// represented by this in-memory view and require separate revocation.
    pub fn narrow_policy(&mut self, requested: &Policy) -> Result<()> {
        self.open()?;
        self.policy = self.policy.narrow(requested)?;
        Ok(())
    }

    /// Effective policy.
    #[must_use]
    pub fn policy(&self) -> &Policy {
        &self.policy
    }
    /// Logical cwd.
    #[must_use]
    pub fn cwd(&self) -> &VirtualPath {
        &self.cwd
    }
    /// Logical tmp root.
    #[must_use]
    pub fn tmp_root(&self) -> VirtualPath {
        VirtualPath::parse("/tmp").expect("static path")
    }
    /// Total stored bytes.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
    /// Whether the view was closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn open(&self) -> Result<()> {
        if self.closed {
            Err(Error::stale_handle("view is closed"))
        } else {
            Ok(())
        }
    }

    /// Resolve `raw` against the cwd. `..` is always rejected.
    pub fn resolve(&self, raw: &str) -> Result<VirtualPath> {
        if raw.is_empty() || raw.contains('\0') || raw.contains('\\') {
            return Err(Error::invalid_path("malformed path"));
        }
        if raw.starts_with('/') {
            return VirtualPath::parse(raw);
        }
        let mut out = self.cwd.clone();
        for seg in raw.split('/') {
            if seg.is_empty() || seg == "." {
                continue;
            }
            if seg == ".." {
                return Err(Error::invalid_path("traversal rejected"));
            }
            out = out.join_segment(seg)?;
        }
        Ok(out)
    }

    /// Read a file after policy check.
    pub fn read(&self, raw: &str) -> Result<Vec<u8>> {
        self.open()?;
        let p = self.resolve(raw)?;
        self.policy.check_read(&p)?;
        self.files
            .get(&p)
            .cloned()
            .ok_or_else(|| Error::invalid_argument("no such file"))
    }

    /// Write a file after policy check and bounded-allocation checks.
    pub fn write(&mut self, raw: &str, data: &[u8]) -> Result<()> {
        self.open()?;
        let p = self.resolve(raw)?;
        self.policy.check_write(&p)?;
        if self.dirs.contains(&p) {
            return Err(Error::invalid_argument("path is a directory"));
        }
        if data.len() > self.limits.max_file_bytes {
            return Err(Error::limit_exceeded("file too large"));
        }
        let old = self.files.get(&p).map_or(0, Vec::len);
        let new_total = self
            .total_bytes
            .checked_sub(old)
            .and_then(|t| t.checked_add(data.len()))
            .ok_or_else(|| Error::overflow("byte accounting overflow"))?;
        if new_total > self.limits.max_total_bytes {
            return Err(Error::limit_exceeded("view byte limit exceeded"));
        }
        if old == 0 && !self.files.contains_key(&p) && self.files.len() >= self.limits.max_files {
            return Err(Error::limit_exceeded("file count limit exceeded"));
        }
        let mut missing = Vec::new();
        let mut parent = p.parent();
        while let Some(dir) = parent {
            if self.files.contains_key(&dir) {
                return Err(Error::invalid_argument("ancestor is a file"));
            }
            if !self.dirs.contains(&dir) {
                missing.push(dir.clone());
            }
            parent = dir.parent();
        }
        if self
            .dirs
            .len()
            .checked_add(missing.len())
            .is_none_or(|n| n > self.limits.max_dirs)
        {
            return Err(Error::limit_exceeded("directory limit exceeded"));
        }
        self.dirs.extend(missing);
        self.total_bytes = new_total;
        self.files.insert(p, data.to_vec());
        Ok(())
    }

    /// Create a directory after policy check.
    pub fn mkdir(&mut self, raw: &str) -> Result<()> {
        self.open()?;
        let p = self.resolve(raw)?;
        self.policy.check_write(&p)?;
        if self.files.contains_key(&p) {
            return Err(Error::invalid_argument("path is a file"));
        }
        let mut parent = p.parent();
        while let Some(dir) = parent {
            if self.files.contains_key(&dir) {
                return Err(Error::invalid_argument("ancestor is a file"));
            }
            parent = dir.parent();
        }
        if self.dirs.len() >= self.limits.max_dirs && !self.dirs.contains(&p) {
            return Err(Error::limit_exceeded("directory limit exceeded"));
        }
        self.dirs.insert(p);
        Ok(())
    }

    /// Remove a file after write-policy check.
    pub fn remove(&mut self, raw: &str) -> Result<()> {
        self.open()?;
        let p = self.resolve(raw)?;
        self.policy.check_write(&p)?;
        let data = self
            .files
            .remove(&p)
            .ok_or_else(|| Error::invalid_argument("no such file"))?;
        self.total_bytes = self.total_bytes.saturating_sub(data.len());
        Ok(())
    }

    /// Whether an authorized path exists; false for denied paths or closed views.
    #[must_use]
    pub fn exists(&self, raw: &str) -> bool {
        self.open()
            .and_then(|()| self.resolve(raw))
            .and_then(|p| {
                self.policy.check_read(&p)?;
                Ok(self.files.contains_key(&p) || self.dirs.contains(&p))
            })
            .unwrap_or(false)
    }

    /// List directory entry names, sorted.
    pub fn list(&self, raw: &str) -> Result<Vec<String>> {
        self.open()?;
        let d = self.resolve(raw)?;
        self.policy.check_read(&d)?;
        if !self.dirs.contains(&d) {
            return Err(Error::invalid_argument("no such directory"));
        }
        let mut names: Vec<String> = self
            .files
            .keys()
            .chain(self.dirs.iter())
            .filter(|p| p.parent().as_ref() == Some(&d))
            .filter_map(|p| p.file_name().map(str::to_owned))
            .collect();
        names.sort();
        Ok(names)
    }

    /// Change logical cwd to an existing directory.
    pub fn set_cwd(&mut self, raw: &str) -> Result<()> {
        self.open()?;
        let p = self.resolve(raw)?;
        self.policy.check_read(&p)?;
        if !self.dirs.contains(&p) {
            return Err(Error::invalid_argument("no such directory"));
        }
        self.cwd = p;
        Ok(())
    }

    /// Trusted embedder hook after validating a directory in an external mount.
    /// Guest callers must use the host context's mounted-directory checks.
    pub fn set_external_cwd(&mut self, path: VirtualPath) -> Result<()> {
        self.open()?;
        self.policy.check_read(&path)?;
        self.cwd = path;
        Ok(())
    }

    /// Set a logical env var. Requires the `env` capability.
    pub fn env_set(&mut self, name: &str, value: &str) -> Result<()> {
        self.open()?;
        self.policy.check_capability("env")?;
        if name.is_empty()
            || name.len() > 64
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(Error::invalid_argument("invalid env name"));
        }
        if value.contains('\0') {
            return Err(Error::invalid_argument("env value contains NUL"));
        }
        if value.len() > self.limits.max_env_value_bytes {
            return Err(Error::limit_exceeded("env value too large"));
        }
        if !self.env.contains_key(name) && self.env.len() >= self.limits.max_env_entries {
            return Err(Error::limit_exceeded("env entry limit exceeded"));
        }
        self.env.insert(name.to_string(), value.to_string());
        Ok(())
    }

    /// Read a logical env var. Requires the `env` capability.
    pub fn env_get(&self, name: &str) -> Result<Option<String>> {
        self.open()?;
        self.policy.check_capability("env")?;
        Ok(self.env.get(name).cloned())
    }

    /// Close the view; every later operation fails.
    pub fn close(&mut self) {
        self.closed = true;
        self.files.clear();
        self.dirs.clear();
        self.env.clear();
        self.total_bytes = 0;
    }

    /// Explicitly unsupported in this milestone.
    pub fn symlink(&self, _target: &str, _link: &str) -> Result<()> {
        Err(Error::unsupported("symlinks unsupported until race-safe"))
    }
    /// Explicitly unsupported in this milestone.
    pub fn mount_host(&self, _host_path: &str, _at: &str) -> Result<()> {
        Err(Error::unsupported(
            "host mounts unsupported until race-safe",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    fn view(n: u64, caps: bool) -> ResourceView {
        let mut p = Policy::default()
            .allow_read("/")
            .unwrap()
            .allow_write("/work")
            .unwrap();
        if caps {
            p = p.allow_capability("env").unwrap();
        }
        ResourceView::new(SandboxId::from_host_parts(9, n), p, ViewLimits::default())
    }

    #[test]
    fn sibling_views_are_isolated() {
        let mut a = view(1, true);
        let mut b = view(2, true);
        a.write("/work/x", b"secret").unwrap();
        a.env_set("TOKEN", "a").unwrap();
        assert!(a.read("/work/x").is_ok());
        assert!(b.read("/work/x").is_err());
        assert_eq!(b.env_get("TOKEN").unwrap(), None);
        b.env_set("TOKEN", "b").unwrap();
        assert_eq!(a.env_get("TOKEN").unwrap().as_deref(), Some("a"));
    }

    #[test]
    fn traversal_and_outside_writes_rejected() {
        let mut v = view(3, false);
        for bad in ["../etc/passwd", "/work/../etc", "a/../../b", "\\\\host\\x"] {
            assert!(v.read(bad).is_err(), "read {bad}");
        }
        assert_eq!(
            v.write("/etc/passwd", b"x").unwrap_err().kind(),
            ErrorKind::Denied
        );
        assert!(v.write("/work/../etc/x", b"x").is_err());
        v.write("/work/a/b.txt", b"ok").unwrap();
        assert!(v.exists("/work/a/b.txt"));
        v.set_cwd("/work/a").unwrap();
        assert_eq!(v.resolve("b.txt").unwrap().as_str(), "/work/a/b.txt");
    }

    #[test]
    fn nested_grant_creates_bounded_parents() {
        let policy = Policy::default().allow_write("/work/deep").unwrap();
        let mut v = ResourceView::new(
            SandboxId::from_host_parts(9, 6),
            policy,
            ViewLimits::default(),
        );
        v.write("/work/deep/a/b.txt", b"ok").unwrap();
        assert!(!v.exists("/work/deep/a/b.txt")); // write grant does not grant metadata reads
        assert!(v.write("/work/other.txt", b"no").is_err());
    }

    #[test]
    fn env_denied_without_capability() {
        let mut v = view(4, false);
        assert_eq!(v.env_set("A", "b").unwrap_err().kind(), ErrorKind::Denied);
        assert!(v.env_get("A").is_err());
    }

    #[test]
    fn bounded_writes_and_close() {
        let limits = ViewLimits {
            max_file_bytes: 4,
            max_total_bytes: 6,
            ..ViewLimits::default()
        };
        let policy = Policy::default().allow_write("/w").unwrap();
        let mut v = ResourceView::new(SandboxId::from_host_parts(9, 5), policy, limits);
        assert_eq!(
            v.write("/w/a", b"12345").unwrap_err().kind(),
            ErrorKind::LimitExceeded
        );
        v.write("/w/a", b"1234").unwrap();
        assert_eq!(
            v.write("/w/b", b"123").unwrap_err().kind(),
            ErrorKind::LimitExceeded
        );
        v.write("/w/b", b"12").unwrap();
        v.close();
        assert!(v.read("/w/a").is_err());
        assert_eq!(
            v.symlink("/x", "/y").unwrap_err().kind(),
            ErrorKind::Unsupported
        );
        assert_eq!(
            v.mount_host("/etc", "/m").unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }
}
