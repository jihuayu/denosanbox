//! A trusted, per-parent broker; guest identifiers have authority only in this map.
use crate::{EngineLimits, Error, Result, SandboxOptions, WorkerHandle};
use denosanbox_core::host::WorkerHandler;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
};

pub(crate) type SpawnWorker = Arc<
    dyn Fn(String, SandboxOptions) -> Pin<Box<dyn Future<Output = Result<WorkerHandle>> + Send>>
        + Send
        + Sync,
>;

struct State {
    children: BTreeMap<String, WorkerHandle>,
    next: u64,
    pending: usize,
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

// A cancelled spawn future releases its pending reservation; Engine::create's
// startup guard separately owns and tears down any partially initialized isolate.
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
            children: BTreeMap::new(),
            next: 0,
            pending: 0,
        }),
    });
    Arc::new(move |operation, input| {
        let broker = broker.clone();
        Box::pin(async move { broker.operation(&operation, input).await })
    })
}

impl Broker {
    async fn close(&self, id: &str, worker: WorkerHandle) -> Result<()> {
        let report = worker.close().await?;
        if !report.closed_cleanly {
            // Retain the owner and its accounting while host work remains live.
            return Err(Error::Policy {
                kind: "IncompleteClose".into(),
                message: "Worker still owns unfinished host work".into(),
            });
        }
        lock(&self.state)?.children.remove(id);
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
            "spawn" => {
                allowed_fields(&input, &["specifier", "options"])?;
                let path = entrypoint(&self.base, string_field(&input, "specifier")?)?;
                if let Some(options) = input.get("options") {
                    allowed_fields(options, &["type", "name"])?;
                    if options
                        .get("type")
                        .is_some_and(|kind| kind.as_str() != Some("module"))
                    {
                        return Err(Error::UnsupportedFeature {
                            feature: "classic_workers".into(),
                        });
                    }
                    if options
                        .get("name")
                        .is_some_and(|name| name.as_str().is_none_or(|name| name.len() > 128))
                    {
                        return Err(invalid("invalid Worker name"));
                    }
                }
                {
                    let mut state = lock(&self.state)?;
                    if state.children.len() + state.pending >= self.limits.max_active_sandboxes {
                        return Err(Error::QueueFull {
                            capacity: self.limits.max_active_sandboxes,
                        });
                    }
                    state.pending += 1;
                }
                let mut reservation = Pending(self.clone(), true);
                let worker = (self.spawn)(path, SandboxOptions::default()).await?;
                let id = {
                    let mut state = lock(&self.state)?;
                    let next = state
                        .next
                        .checked_add(1)
                        .ok_or_else(|| Error::LimitExceeded {
                            limit: "worker_identifiers".into(),
                            detail: "Worker identifier space exhausted".into(),
                        })?;
                    state.next = next;
                    let id = next.to_string();
                    state.children.insert(id.clone(), worker);
                    state.pending -= 1;
                    reservation.1 = false;
                    id
                };
                Ok(json!(id))
            }
            "post" => {
                allowed_fields(&input, &["id", "message"])?;
                let id = string_field(&input, "id")?.to_owned();
                let message = input
                    .get("message")
                    .cloned()
                    .ok_or_else(|| invalid("missing Worker message"))?;
                let worker = lock(&self.state)?
                    .children
                    .get(&id)
                    .cloned()
                    .ok_or(Error::Closed)?;
                let reply = worker.post_message(message).await?;
                if let Some(messages) = reply.get("__workerMessages") {
                    if messages
                        .as_array()
                        .is_none_or(|messages| messages.len() > 32)
                    {
                        return Err(Error::LimitExceeded {
                            limit: "worker_reply_batch".into(),
                            detail: "invalid Worker reply batch".into(),
                        });
                    }
                    if reply.get("closed").and_then(Value::as_bool) == Some(true) {
                        self.close(&id, worker).await?;
                    }
                }
                Ok(reply)
            }
            "terminate" => {
                allowed_fields(&input, &["id"])?;
                let id = string_field(&input, "id")?;
                let worker = lock(&self.state)?
                    .children
                    .get(id)
                    .cloned()
                    .ok_or(Error::Closed)?;
                self.close(id, worker).await?;
                Ok(Value::Null)
            }
            _ => Err(Error::UnsupportedFeature {
                feature: format!("Worker operation {operation}"),
            }),
        }
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
