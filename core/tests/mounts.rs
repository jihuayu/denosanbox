#![cfg(all(feature = "deno", target_os = "linux"))]
//! Real V8 -> mediated guest filesystem -> host directory capability tests.
use denosanbox::{
    Engine, EngineConfig, InvocationOptions, MountAccess, PluginSource, Policy, SandboxHandle,
    SandboxOptions,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(1);
struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "denosanbox-guest-mount-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
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
async fn sandbox(engine: &Engine, policy: Policy) -> SandboxHandle {
    let plugin = engine.prepare(PluginSource::single("main.js", r#"
        export async function read(path) { return await sandbox.fs.readTextFile(path); }
        export async function write(input) { await sandbox.fs.writeTextFile(input.path, input.data); return "ok"; }
        export function cwd() { return sandbox.cwd(); }
        export async function cd(path) { await sandbox.chdir(path); return await sandbox.cwd(); }
    "#)).unwrap();
    engine
        .create(
            plugin,
            SandboxOptions {
                policy,
                ..Default::default()
            },
        )
        .await
        .unwrap()
}
async fn read(sandbox: &SandboxHandle, path: &str) -> denosanbox::Result<Value> {
    sandbox
        .call("read", json!(path), InvocationOptions::default())
        .await
}
async fn write(sandbox: &SandboxHandle, path: &str, data: &str) -> denosanbox::Result<Value> {
    sandbox
        .call(
            "write",
            json!({"path":path,"data":data}),
            InvocationOptions::default(),
        )
        .await
}

#[tokio::test]
async fn readonly_mounts_do_not_allow_guest_writes_or_ambient_reads() {
    let dir = TestDir::new();
    std::fs::write(dir.path().join("file"), "allowed").unwrap();
    let engine = Engine::with_policy(EngineConfig::default(), policy()).unwrap();
    let guest = sandbox(&engine, policy()).await;
    guest
        .mount_directory("/mount", dir.path(), MountAccess::ReadOnly)
        .unwrap();
    assert_eq!(read(&guest, "/mount/file").await.unwrap(), json!("allowed"));
    assert_eq!(read(&guest, "mount/file").await.unwrap(), json!("allowed"));
    let host_cwd = std::env::current_dir().unwrap();
    assert_eq!(
        guest
            .call("cd", json!("/mount"), InvocationOptions::default())
            .await
            .unwrap(),
        json!("/mount")
    );
    assert_eq!(read(&guest, "file").await.unwrap(), json!("allowed"));
    assert_eq!(std::env::current_dir().unwrap(), host_cwd);
    assert!(write(&guest, "/mount/file", "corrupted").await.is_err());
    assert!(read(&guest, "/etc/passwd").await.is_err());
    assert!(read(&guest, "/mount/../etc/passwd").await.is_err());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("file")).unwrap(),
        "allowed"
    );
    assert!(guest.close().await.unwrap().closed_cleanly);
    assert!(engine.shutdown().await.unwrap().clean);
}

#[tokio::test]
async fn mounted_views_are_private_and_live_policy_narrowing_rechecks_operations() {
    let first = TestDir::new();
    let second = TestDir::new();
    std::fs::write(first.path().join("file"), "first").unwrap();
    std::fs::write(second.path().join("file"), "second").unwrap();
    let engine = Engine::with_policy(EngineConfig::default(), policy()).unwrap();
    let a = sandbox(&engine, policy()).await;
    let b = sandbox(&engine, policy()).await;
    a.mount_directory("/mount", first.path(), MountAccess::ReadWrite)
        .unwrap();
    b.mount_directory("/mount", second.path(), MountAccess::ReadWrite)
        .unwrap();
    assert_eq!(read(&a, "/mount/file").await.unwrap(), json!("first"));
    assert_eq!(read(&b, "/mount/file").await.unwrap(), json!("second"));
    write(&a, "/mount/file", "changed").await.unwrap();
    assert_eq!(read(&b, "/mount/file").await.unwrap(), json!("second"));
    a.narrow_policy(&Policy::default().allow_read("/mount").unwrap())
        .unwrap();
    assert!(write(&a, "/mount/file", "denied").await.is_err());
    assert_eq!(read(&a, "/mount/file").await.unwrap(), json!("changed"));
    a.narrow_policy(&Policy::default()).unwrap();
    assert!(read(&a, "/mount/file").await.is_err());
    assert_eq!(
        std::fs::read_to_string(first.path().join("file")).unwrap(),
        "changed"
    );
    assert!(engine.shutdown().await.unwrap().clean);
}

#[tokio::test]
async fn guest_symlink_paths_cannot_alias_other_policy_subtrees_or_host_files() {
    use std::os::unix::fs::symlink;
    let root = TestDir::new();
    let outside = TestDir::new();
    std::fs::create_dir(root.path().join("public")).unwrap();
    std::fs::create_dir(root.path().join("private")).unwrap();
    std::fs::write(root.path().join("private/file"), "inside-secret").unwrap();
    std::fs::write(outside.path().join("file"), "outside-secret").unwrap();
    symlink("../private", root.path().join("public/alias")).unwrap();
    symlink(outside.path(), root.path().join("public/escape")).unwrap();
    symlink(outside.path().join("file"), root.path().join("public/leaf")).unwrap();
    let effective = Policy::default()
        .allow_read("/mount/public")
        .unwrap()
        .allow_write("/mount/public")
        .unwrap();
    let engine = Engine::with_policy(EngineConfig::default(), effective.clone()).unwrap();
    let guest = sandbox(&engine, effective).await;
    guest
        .mount_directory("/mount", root.path(), MountAccess::ReadWrite)
        .unwrap();
    for path in [
        "/mount/public/alias/file",
        "/mount/public/escape/file",
        "/mount/public/leaf",
    ] {
        assert!(read(&guest, path).await.is_err(), "{path}");
        assert!(write(&guest, path, "corrupted").await.is_err(), "{path}");
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("private/file")).unwrap(),
        "inside-secret"
    );
    assert_eq!(
        std::fs::read_to_string(outside.path().join("file")).unwrap(),
        "outside-secret"
    );
    assert!(engine.shutdown().await.unwrap().clean);
}
