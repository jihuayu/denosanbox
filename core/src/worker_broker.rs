//! A trusted, per-parent broker; guest identifiers have authority only in this map.
//!
//! The broker is installed on a [`HostContext`] by Rust before the isolate
//! thread starts. Every operation is re-validated here: entries resolve inside
//! the parent's immutable prepared graph, `spawn` always goes through
//! `Engine::create_child`, and `post`/`terminate` are the only operations that
//! map a guest identifier to a child.
use crate::{EngineLimits, Error, Result, SandboxOptions, WorkerHandle};
use denosanbox_core::WorkerHandler;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
};

/// Maximum message batch a child may return from one `onmessage` invocation.
pub(crate) const MAX_REPLY_BATCH: usize = 32;
/// Maximum queued/in-flight parent messages per child.
pub(crate) const MAX_QUEUED_MESSAGES: usize = 32;
/// Per-child byte cap for parent messages.
pub(crate) const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;
/// Timeout for a single parent-message invocation.
pub(crate) const MESSAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(crate) type SpawnWorker = Arc<
    dyn Fn(String, SandboxOptions) -> Pin<Box<dyn Future<Output = Result<WorkerHandle>> + Send>>
        + Send
        + Sync,
>;

struct State {
    /// Children created by a successful spawn whose guest handle is not acked.
    staged: BTreeMap<String, WorkerHandle>,
    /// Wakes `post` callers waiting for each staged id to be acked.
    ready: BTreeMap<String, Arc<tokio::sync::Notify>>,
    /// Children whose spawn was acknowledged to the guest.
    children: BTreeMap<String, WorkerHandle>,
    next: u64,
    pending: usize,
    /// Spawn slots reserved for acknowledged-but-unterminated guest handles.
    admitted: usize,
}

impl State {
    fn live(&self) -> usize {
        self.staged.len() + self.children.len()
    }
}

struct Broker {
    spawn: SpawnWorker,
    base: String,
    limits: EngineLimits,
    state: Mutex<State>,
}

fn invalid(reason: &str) -> Error {
    Error::InvalidConfiguration {
        reason: reason.into(),
    }
}
fn lock(state: &Mutex<State>) -> Result<MutexGuard<'_, State>> {
    state.lock().map_err(|_| Error::Internal {
        message: "Worker broker lock poisoned".into(),
    })
}
fn string_field<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("invalid Worker protocol field"))
}
fn allowed_fields(value: &Value, names: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("Worker request must be an object"))?;
    if object.keys().any(|name| !names.contains(&name.as_str())) {
        return Err(invalid("unknown Worker protocol field"));
    }
    Ok(())
}

/// Resolve inside the parent's package; no filesystem or URL lookup occurs here.
fn entrypoint(base: &str, specifier: &str) -> Result<String> {
    if specifier.is_empty()
        || specifier.len() > 4096
        || specifier.starts_with('/')
        || specifier
            .chars()
            .any(|c| c.is_control() || matches!(c, ':' | '\\' | '?' | '#' | '%'))
    {
        return Err(Error::ModuleDenied {
            specifier: specifier.into(),
            reason: "unsupported Worker entrypoint".into(),
        });
    }
    let mut parts: Vec<&str> = base
        .rsplit_once('/')
        .map(|(directory, _)| directory.split('/').collect())
        .unwrap_or_default();
    for part in specifier.split('/') {
        match part {
            "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(Error::ModuleDenied {
                        specifier: specifier.into(),
                        reason: "Worker entrypoint escapes package".into(),
                    });
                }
            }
            "" => return Err(invalid("empty Worker path component")),
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err(invalid("empty Worker entrypoint"));
    }
    Ok(parts.join("/"))
}

/// A pending spawn reservation. Dropping the future (parent deadline, cancel
/// or saturation) always releases the quota slot exactly once.
struct Pending(Arc<Broker>, bool);
impl Drop for Pending {
    fn drop(&mut self) {
        if self.1 {
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.pending = state.pending.saturating_sub(1);
        }
    }
}

pub(crate) fn handler(
    spawn: SpawnWorker,
    base_entrypoint: String,
    limits: EngineLimits,
) -> WorkerHandler {
    let broker = Arc::new(Broker {
        spawn,
        base: base_entrypoint,
        limits,
        state: Mutex::new(State {
            staged: BTreeMap::new(),
            ready: BTreeMap::new(),
            children: BTreeMap::new(),
            next: 0,
            pending: 0,
            admitted: 0,
        }),
    });
    Arc::new(move |operation: String, input: Value| {
        let broker = broker.clone();
        Box::pin(async move { broker.operation(&operation, input).await })
    })
}

impl Broker {
    async fn close(&self, id: &str, worker: WorkerHandle) -> Result<()> {
        let report = worker.close().await?;
        let mut state = lock(&self.state)?;
        state.children.remove(id);
        state.admitted = state.admitted.saturating_sub(1);
        drop(state);
        if !report.closed_cleanly {
            // Retain the owner and its accounting while host work remains live.
            return Err(Error::Policy {
                kind: "IncompleteClose".into(),
                message: "Worker still owns unfinished host work".into(),
            });
        }
        Ok(())
    }

    async fn operation(self: &Arc<Self>, operation: &str, input: Value) -> Result<Value> {
        let bytes = serde_json::to_vec(&input)
            .map_err(|_| invalid("invalid Worker JSON"))?
            .len();
        if bytes > self.limits.max_message_bytes {
            return Err(Error::LimitExceeded {
                limit: "worker_message_bytes".into(),
                detail: "Worker protocol input is too large".into(),
            });
        }
        match operation {
            "spawn" => self.spawn(&input).await,
            "ack" => self.ack(&input),
            "abort" => self.abort(&input),
            "post" => self.post(&input).await,
            "terminate" => self.terminate(&input).await,
            other => Err(Error::UnsupportedFeature {
                feature: format!("Worker operation {other}"),
            }),
        }
    }

    async fn spawn(self: &Arc<Self>, input: &Value) -> Result<Value> {
        allowed_fields(input, &["specifier", "options"])?;
        let path = entrypoint(&self.base, string_field(input, "specifier")?)?;
        match input.get("options") {
            Some(options) => {
                allowed_fields(options, &["type", "name"])?;
                match options.get("type") {
                    Some(Value::String(kind)) if kind == "module" => {}
                    Some(Value::String(_)) => {
                        return Err(Error::UnsupportedFeature {
                            feature: "classic_workers".into(),
                        })
                    }
                    Some(_) => return Err(invalid("invalid Worker type")),
                    None => {
                        return Err(Error::UnsupportedFeature {
                            feature: "classic_workers".into(),
                        })
                    }
                }
                if options
                    .get("name")
                    .is_some_and(|name| name.as_str().is_none_or(|name| name.len() > 128))
                {
                    return Err(invalid("invalid Worker name"));
                }
            }
            None => {
                return Err(Error::UnsupportedFeature {
                    feature: "classic_workers".into(),
                })
            }
        }
        {
            let mut state = lock(&self.state)?;
            if state.live() + state.pending + state.admitted >= self.limits.max_active_sandboxes {
                return Err(Error::QueueFull {
                    capacity: self.limits.max_active_sandboxes,
                });
            }
            state.pending += 1;
        }
        let mut reservation = Pending(self.clone(), true);
        let mut options = SandboxOptions::default();
        // The inbox allowance is charged on top of the child heap target, which
        // the engine only accepts above its Deno floor (64MiB default).
        options.memory_limit_bytes = Some(
            MAX_QUEUED_BYTES.max(denosanbox_core::policy::Limits::default().heap_target_bytes()),
        );
        let worker = (self.spawn)(path, options).await?;
        let id = {
            let mut state = lock(&self.state)?;
            let next = state
                .next
                .checked_add(1)
                .ok_or(Error::LimitExceeded {
                    limit: "worker_identifiers".into(),
                    detail: "Worker identifier space exhausted".into(),
                })?;
            state.next = next;
            let id = next.to_string();
            state.pending = state.pending.saturating_sub(1);
            state.admitted += 1;
            state.staged.insert(id.clone(), worker);
            state.ready.insert(id.clone(), Arc::new(tokio::sync::Notify::new()));
            reservation.1 = false;
            id
        };
        Ok(json!(id))
    }

    /// Marks a spawn complete once this sandbox has finished starting.
    ///
    /// Called first on the *child's* thread for its own id, then by the parent
    /// right before it delivers the first message; both are idempotent. The
    /// very first message therefore implies "child ready" without the parent
    /// having to suspend on a bare ack round trip.
    fn ack(&self, input: &Value) -> Result<Value> {
        allowed_fields(input, &["id"])?;
        let id = string_field(input, "id")?;
        let mut state = lock(&self.state)?;
        match state.staged.remove(id) {
            Some(worker) => {
                state.children.insert(id.to_owned(), worker);
                if let Some(waiters) = state.ready.remove(id) {
                    waiters.notify_waiters();
                }
                Ok(Value::Null)
            }
            // Already acknowledged, terminated or aborted: idempotent success.
            None => Ok(Value::Null),
        }
    }

    fn abort(&self, input: &Value) -> Result<Value> {
        allowed_fields(input, &["id"])?;
        let id = string_field(input, "id")?;
        let mut state = lock(&self.state)?;
        if let Some(worker) = state.staged.remove(id) {
            state.admitted = state.admitted.saturating_sub(1);
            if let Some(waiters) = state.ready.remove(id) {
                waiters.notify_waiters();
            }
            worker.cancel();
        }
        Ok(Value::Null)
    }

    async fn post(&self, input: &Value) -> Result<Value> {
        allowed_fields(input, &["id", "message"])?;
        let id = string_field(input, "id")?.to_owned();
        let message = input
            .get("message")
            .cloned()
            .ok_or_else(|| invalid("missing Worker message"))?;
        let encoded = serde_json::to_vec(&message)
            .map_err(|_| invalid("invalid Worker JSON"))?
            .len();
        if encoded > self.limits.max_message_bytes {
            return Err(Error::LimitExceeded {
                limit: "worker_message_bytes".into(),
                detail: "Worker message is too large".into(),
            });
        }
        // Resolve without holding the lock across the wait.
        let staged = {
            let state = lock(&self.state)?;
            if state.children.contains_key(&id) {
                None
            } else {
                // Startup not acknowledged yet: wait natively for this same
                // child rather than rejecting or reordering the message.
                let staged = state.staged.get(&id).cloned().ok_or(Error::Closed)?;
                let ready = state.ready.get(&id).cloned().ok_or(Error::Closed)?;
                Some((staged, ready))
            }
        };
        if let Some((staged, ready)) = staged {
            wait_ready(&self.state, &id, &ready).await?;
            // Startup is over: promote to the acknowledged map before the
            // message is delivered. A spawn that was aborted meanwhile fails
            // with `Closed` instead of delivering.
            let promoted = {
                let mut state = lock(&self.state)?;
                if state.staged.remove(&id).is_some() {
                    state.children.insert(id.clone(), staged);
                    true
                } else {
                    false
                }
            };
            if !promoted {
                return Err(Error::Closed);
            }
        }
        let worker = lock(&self.state)?
            .children
            .get(&id)
            .cloned()
            .ok_or_else(|| {
                Error::Closed
            })?;
        let reply = worker.post_message(message).await?;
        if let Some(messages) = reply.get("__workerMessages") {
            if messages
                .as_array()
                .is_none_or(|messages| messages.len() > MAX_REPLY_BATCH)
            {
                return Err(Error::LimitExceeded {
                    limit: "worker_reply_batch".into(),
                    detail: "invalid Worker reply batch".into(),
                });
            }
        }
        if reply
            .get("closed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let still = lock(&self.state)?.children.contains_key(&id);
            if still {
                self.close(&id, worker).await?;
            }
        }
        Ok(reply)
    }

    async fn terminate(&self, input: &Value) -> Result<Value> {
        allowed_fields(input, &["id"])?;
        let id = string_field(input, "id")?.to_owned();
        let (staged, worker) = {
            let mut state = lock(&self.state)?;
            if let Some(worker) = state.staged.remove(&id) {
                state.admitted = state.admitted.saturating_sub(1);
                (true, worker)
            } else {
                match state.children.get(&id) {
                    Some(worker) => (false, worker.clone()),
                    None => return Ok(Value::Null),
                }
            }
        };
        if staged {
            worker.cancel();
            return Ok(Value::Null);
        }
        self.close(&id, worker).await?;
        Ok(Value::Null)
    }
}

/// Waits until `id` is acknowledged or removed (abort/terminate). The child
/// handle captured by the caller keeps the pending startup itself alive.
async fn wait_ready(state: &Mutex<State>, id: &str, notify: &tokio::sync::Notify) -> Result<()> {
    loop {
        let notified = notify.notified();
        if !lock(state)?.ready.contains_key(id) {
            return Ok(());
        }
        tokio::time::timeout(std::time::Duration::from_millis(10), notified)
            .await
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::entrypoint;
    #[test]
    fn entrypoints_are_virtual_and_cannot_escape_the_package() {
        assert_eq!(
            entrypoint("lib/parent.js", "./child.js").unwrap(),
            "lib/child.js"
        );
        assert_eq!(
            entrypoint("lib/parent.js", "../child.js").unwrap(),
            "child.js"
        );
        for path in [
            "../../host.js",
            "file:///etc/passwd",
            "https://example.com/x.js",
            "/host.js",
            "%2e%2e/host.js",
            "child.js?x",
            "a\\b.js",
        ] {
            assert!(entrypoint("lib/parent.js", path).is_err(), "{path}");
        }
    }
}
