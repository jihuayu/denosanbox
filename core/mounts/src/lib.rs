//! Explicit Linux host-directory capabilities for sandbox virtual paths.
//! All guest path components reject symlinks. No host cwd or environment changes.
#![forbid(unsafe_code)]
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use denosanbox_policy::{Policy, SandboxId, VirtualPath};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;

/// Allowed operations on one explicitly granted directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountAccess {
    ReadOnly,
    ReadWrite,
}
/// Guest-safe failures never include ambient host paths or OS error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    WrongOwner,
    Closed,
    InvalidPath,
    Denied,
    NotMounted,
    ReadOnly,
    LimitExceeded,
    Unsupported,
    NotRegularFile,
    Io(io::ErrorKind),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(kind) => write!(f, "mounted resource I/O failed: {kind:?}"),
            other => write!(f, "mounted resource rejected: {other:?}"),
        }
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e.kind())
    }
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
struct Mount {
    dir: Dir,
    access: MountAccess,
}
/// A sandbox-owned set of non-overlapping directory capabilities.
#[derive(Debug)]
pub struct MountTable {
    owner: SandboxId,
    mounts: BTreeMap<VirtualPath, Mount>,
    max_mounts: usize,
    max_file_bytes: usize,
    closed: bool,
}
impl MountTable {
    pub fn new(owner: SandboxId, max_mounts: usize, max_file_bytes: usize) -> Self {
        Self {
            owner,
            mounts: BTreeMap::new(),
            max_mounts,
            max_file_bytes,
            closed: false,
        }
    }
    fn check(&self, owner: SandboxId) -> Result<()> {
        if owner != self.owner {
            return Err(Error::WrongOwner);
        }
        if self.closed {
            return Err(Error::Closed);
        }
        Ok(())
    }
    /// Trusted-host setup only. The opened directory handle anchors authority;
    /// later host renames do not revoke that object capability.
    pub fn add_mount(
        &mut self,
        owner: SandboxId,
        prefix: &str,
        host_path: &Path,
        access: MountAccess,
    ) -> Result<()> {
        self.check(owner)?;
        if !cfg!(target_os = "linux") {
            return Err(Error::Unsupported);
        }
        let prefix = VirtualPath::parse(prefix).map_err(|_| Error::InvalidPath)?;
        if self.mounts.len() >= self.max_mounts {
            return Err(Error::LimitExceeded);
        }
        if self
            .mounts
            .keys()
            .any(|p| p.is_ancestor_or_equal(&prefix) || prefix.is_ancestor_or_equal(p))
        {
            return Err(Error::Denied);
        }
        let dir = Dir::open_ambient_dir(host_path, cap_std::ambient_authority())?;
        self.mounts.insert(prefix, Mount { dir, access });
        Ok(())
    }
    /// Number of retained directory capabilities.
    pub fn len(&self) -> usize {
        self.mounts.len()
    }
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }
    /// Whether the path belongs to a live mount; invalid paths never match.
    pub fn contains(&self, path: &str) -> bool {
        if self.closed {
            return false;
        }
        let Ok(path) = VirtualPath::parse(path) else {
            return false;
        };
        self.mounts.keys().any(|p| p.is_ancestor_or_equal(&path))
    }
    /// Clone retained capabilities for a host-created child. The child must use
    /// its own effective policy on every operation; no policy is widened here.
    pub fn clone_for(&self, owner: SandboxId) -> Result<Self> {
        if self.closed {
            return Err(Error::Closed);
        }
        let mut mounts = BTreeMap::new();
        for (prefix, mount) in &self.mounts {
            mounts.insert(
                prefix.clone(),
                Mount {
                    dir: mount.dir.try_clone()?,
                    access: mount.access,
                },
            );
        }
        Ok(Self {
            owner,
            mounts,
            max_mounts: self.max_mounts,
            max_file_bytes: self.max_file_bytes,
            closed: false,
        })
    }
    fn parent_and_leaf(&self, path: &VirtualPath) -> Result<(Dir, String, MountAccess)> {
        let (prefix, mount) = self
            .mounts
            .iter()
            .find(|(p, _)| p.is_ancestor_or_equal(path))
            .ok_or(Error::NotMounted)?;
        let mut parts: Vec<_> = path.segments().skip(prefix.depth()).collect();
        let leaf = parts.pop().ok_or(Error::NotRegularFile)?.to_owned();
        let mut dir = mount.dir.try_clone()?;
        // One checked component per handle-relative open: no canonicalize/open
        // race and no symlink alias to another policy subtree, even inside root.
        for part in parts {
            dir = dir.open_dir_nofollow(part)?;
        }
        Ok((dir, leaf, mount.access))
    }
    pub fn read(&self, owner: SandboxId, policy: &Policy, path: &str) -> Result<Vec<u8>> {
        self.check(owner)?;
        let path = VirtualPath::parse(path).map_err(|_| Error::InvalidPath)?;
        policy.check_read(&path).map_err(|_| Error::Denied)?;
        let (dir, leaf, _) = self.parent_and_leaf(&path)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        nonblocking(&mut options);
        let file = dir.open_with(leaf, &options)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(Error::NotRegularFile);
        }
        if metadata.len() > self.max_file_bytes as u64 {
            return Err(Error::LimitExceeded);
        }
        let limit = self
            .max_file_bytes
            .checked_add(1)
            .ok_or(Error::LimitExceeded)?;
        let mut bytes = Vec::new();
        file.take(limit as u64).read_to_end(&mut bytes)?;
        if bytes.len() > self.max_file_bytes {
            return Err(Error::LimitExceeded);
        }
        Ok(bytes)
    }
    /// Writes are bounded but not transactional. A returned I/O error can occur
    /// after truncation or partial writing; cancellation does not roll it back.
    pub fn write(&self, owner: SandboxId, policy: &Policy, path: &str, data: &[u8]) -> Result<()> {
        self.check(owner)?;
        if data.len() > self.max_file_bytes {
            return Err(Error::LimitExceeded);
        }
        let path = VirtualPath::parse(path).map_err(|_| Error::InvalidPath)?;
        policy.check_write(&path).map_err(|_| Error::Denied)?;
        let (dir, leaf, access) = self.parent_and_leaf(&path)?;
        if access != MountAccess::ReadWrite {
            return Err(Error::ReadOnly);
        }
        let mut options = OpenOptions::new();
        options.write(true).create(true).follow(FollowSymlinks::No);
        nonblocking(&mut options);
        let mut file = dir.open_with(leaf, &options)?;
        if !file.metadata()?.is_file() {
            return Err(Error::NotRegularFile);
        }
        file.set_len(0)?;
        file.write_all(data)?;
        Ok(())
    }
    /// Idempotently release this owner's retained directory handles.
    pub fn close(&mut self, owner: SandboxId) -> Result<()> {
        if owner != self.owner {
            return Err(Error::WrongOwner);
        }
        self.closed = true;
        self.mounts.clear();
        Ok(())
    }
}
fn nonblocking(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    let _ = options;
}
