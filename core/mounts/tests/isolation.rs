#![cfg(target_os = "linux")]
use denosanbox_mounts::{MountAccess, MountTable};
use denosanbox_policy::{Policy, SandboxId};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
static NEXT: AtomicU64 = AtomicU64::new(1);
struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "denosanbox-mount-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn policy() -> Policy {
    Policy::default()
        .allow_read("/mount")
        .unwrap()
        .allow_write("/mount")
        .unwrap()
}

#[test]
fn explicit_grants_ownership_narrowing_and_close_are_enforced() {
    let dir = TestDir::new();
    std::fs::write(dir.path().join("file"), b"public").unwrap();
    let owner = SandboxId::mint();
    let other = SandboxId::mint();
    let mut mounts = MountTable::new(owner, 2, 64);
    mounts
        .add_mount(owner, "/mount", dir.path(), MountAccess::ReadOnly)
        .unwrap();
    assert!(mounts.contains("/mount/file"));
    assert!(!mounts.contains("/mountain/file"));
    assert_eq!(
        mounts.read(owner, &policy(), "/mount/file").unwrap(),
        b"public"
    );
    assert!(mounts.read(other, &policy(), "/mount/file").is_err());
    assert!(mounts
        .write(owner, &policy(), "/mount/file", b"bad")
        .is_err());
    assert!(mounts
        .read(owner, &Policy::default(), "/mount/file")
        .is_err());
    assert!(mounts.read(owner, &policy(), "/mount/../file").is_err());
    assert!(mounts
        .add_mount(owner, "/mount/sub", dir.path(), MountAccess::ReadOnly)
        .is_err());
    assert!(mounts.close(other).is_err());
    mounts.close(owner).unwrap();
    mounts.close(owner).unwrap();
    assert!(mounts.read(owner, &policy(), "/mount/file").is_err());
    assert!(mounts
        .add_mount(owner, "/again", dir.path(), MountAccess::ReadWrite)
        .is_err());
}

#[test]
fn write_and_read_limits_do_not_mutate_oversized_files() {
    let dir = TestDir::new();
    let owner = SandboxId::mint();
    let mut mounts = MountTable::new(owner, 1, 8);
    mounts
        .add_mount(owner, "/mount", dir.path(), MountAccess::ReadWrite)
        .unwrap();
    mounts
        .write(owner, &policy(), "/mount/file", b"hello")
        .unwrap();
    assert_eq!(std::fs::read(dir.path().join("file")).unwrap(), b"hello");
    assert!(mounts
        .write(owner, &policy(), "/mount/file", b"123456789")
        .is_err());
    assert_eq!(std::fs::read(dir.path().join("file")).unwrap(), b"hello");
    std::fs::write(dir.path().join("large"), [0u8; 9]).unwrap();
    assert!(mounts.read(owner, &policy(), "/mount/large").is_err());
    assert!(mounts.read(owner, &policy(), "/mount").is_err());
    assert!(mounts
        .add_mount(owner, "/second", dir.path(), MountAccess::ReadWrite)
        .is_err());
}

#[test]
fn symlinks_cannot_escape_or_alias_a_denied_virtual_subtree() {
    use std::os::unix::fs::symlink;
    let dir = TestDir::new();
    let outside = TestDir::new();
    let owner = SandboxId::mint();
    std::fs::create_dir(dir.path().join("public")).unwrap();
    std::fs::create_dir(dir.path().join("private")).unwrap();
    std::fs::write(dir.path().join("private/secret"), b"inside-secret").unwrap();
    std::fs::write(outside.path().join("secret"), b"outside-secret").unwrap();
    symlink(outside.path(), dir.path().join("public/escape")).unwrap();
    symlink("../private", dir.path().join("public/alias")).unwrap();
    symlink(
        outside.path().join("secret"),
        dir.path().join("public/leaf"),
    )
    .unwrap();
    let mut mounts = MountTable::new(owner, 1, 128);
    mounts
        .add_mount(owner, "/mount", dir.path(), MountAccess::ReadWrite)
        .unwrap();
    let narrow = Policy::default()
        .allow_read("/mount/public")
        .unwrap()
        .allow_write("/mount/public")
        .unwrap();
    for path in [
        "/mount/public/escape/secret",
        "/mount/public/alias/secret",
        "/mount/public/leaf",
    ] {
        assert!(mounts.read(owner, &narrow, path).is_err());
        assert!(mounts.write(owner, &narrow, path, b"corrupted").is_err());
    }
    assert_eq!(
        std::fs::read(outside.path().join("secret")).unwrap(),
        b"outside-secret"
    );
    assert_eq!(
        std::fs::read(dir.path().join("private/secret")).unwrap(),
        b"inside-secret"
    );
}

#[test]
fn directory_substitution_race_never_accesses_outside_grant() {
    use std::os::unix::fs::symlink;
    let dir = TestDir::new();
    let outside = TestDir::new();
    let owner = SandboxId::mint();
    std::fs::create_dir(dir.path().join("race")).unwrap();
    std::fs::write(dir.path().join("race/file"), b"allowed").unwrap();
    std::fs::write(outside.path().join("file"), b"secret").unwrap();
    let mut mounts = MountTable::new(owner, 1, 64);
    mounts
        .add_mount(owner, "/mount", dir.path(), MountAccess::ReadWrite)
        .unwrap();
    assert_eq!(
        mounts.read(owner, &policy(), "/mount/race/file").unwrap(),
        b"allowed"
    );
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let root = dir.path().to_owned();
    let external = outside.path().to_owned();
    let racer = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Relaxed) {
            std::fs::rename(root.join("race"), root.join("held")).unwrap();
            symlink(&external, root.join("race")).unwrap();
            std::thread::yield_now();
            std::fs::remove_file(root.join("race")).unwrap();
            std::fs::rename(root.join("held"), root.join("race")).unwrap();
        }
    });
    for _ in 0..1000 {
        if let Ok(data) = mounts.read(owner, &policy(), "/mount/race/file") {
            assert_eq!(data, b"allowed");
        }
        let _ = mounts.write(owner, &policy(), "/mount/race/file", b"allowed");
    }
    stop.store(true, Ordering::Relaxed);
    racer.join().unwrap();
    assert_eq!(
        std::fs::read(outside.path().join("file")).unwrap(),
        b"secret"
    );
}

#[test]
fn inherited_handles_are_bound_to_new_owner_and_new_policy() {
    let dir = TestDir::new();
    let parent = SandboxId::mint();
    let child = SandboxId::mint();
    std::fs::write(dir.path().join("file"), b"allowed").unwrap();
    let mut mounts = MountTable::new(parent, 1, 64);
    mounts
        .add_mount(parent, "/mount", dir.path(), MountAccess::ReadWrite)
        .unwrap();
    let mut inherited = mounts.clone_for(child).unwrap();
    assert!(inherited.read(parent, &policy(), "/mount/file").is_err());
    assert!(inherited
        .read(child, &Policy::default(), "/mount/file")
        .is_err());
    assert_eq!(
        inherited.read(child, &policy(), "/mount/file").unwrap(),
        b"allowed"
    );
    inherited.close(child).unwrap();
    assert_eq!(
        mounts.read(parent, &policy(), "/mount/file").unwrap(),
        b"allowed"
    );
}

#[test]
fn fifo_is_rejected_without_waiting_for_a_writer() {
    let dir = TestDir::new();
    let owner = SandboxId::mint();
    assert!(std::process::Command::new("mkfifo")
        .arg(dir.path().join("pipe"))
        .status()
        .unwrap()
        .success());
    let mut mounts = MountTable::new(owner, 1, 64);
    mounts
        .add_mount(owner, "/mount", dir.path(), MountAccess::ReadWrite)
        .unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        sender
            .send(mounts.read(owner, &policy(), "/mount/pipe"))
            .unwrap();
    });
    assert!(receiver
        .recv_timeout(std::time::Duration::from_millis(500))
        .unwrap()
        .is_err());
}
