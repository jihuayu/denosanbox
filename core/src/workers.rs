//! Narrow, host-managed request/reply worker profile.
//!
//! [`WorkerHandle`] wraps one child sandbox created through `Engine::create_child`.
//! It is intentionally *not* a full browser `Worker`, not a Node worker thread and
//! not an OS subprocess: the host owns the supervisor, policy is enforced by the
//! native core, and only JSON values cross the boundary -- no handles or shared
//! memory.

use crate::{
    CloseReport, Engine, Error, InvocationOptions, PreparedPlugin, Result, SandboxHandle,
    SandboxOptions,
};
use serde_json::Value;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Tuning knobs for a [`WorkerHandle`].
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// Maximum number of queued/in-flight messages.
    pub queue_capacity: usize,
    /// Maximum total serialized bytes of queued/in-flight messages.
    pub max_queued_bytes: usize,
    /// Maximum serialized size of a single message.
    pub max_message_bytes: usize,
    /// Timeout applied to each `onmessage` invocation.
    pub invocation_timeout: Duration,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            queue_capacity: 32,
            max_queued_bytes: 4 * 1024 * 1024,
            max_message_bytes: 1024 * 1024,
            invocation_timeout: Duration::from_secs(30),
        }
    }
}

struct WorkerInner {
    child: SandboxHandle,
    options: WorkerOptions,
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    closed: AtomicBool,
}

impl fmt::Debug for WorkerInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerInner")
            .field("options", &self.options)
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// Handle to a host-managed request/reply worker.
#[derive(Debug, Clone)]
pub struct WorkerHandle {
    inner: Arc<WorkerInner>,
}

impl WorkerHandle {
    /// Spawn a worker around a child sandbox derived from `parent`.
    ///
    /// The child plugin graph is derived from the parent's prepared plugin with
    /// `entrypoint` applied; policy, delegation, deadline and budget enforcement
    /// stay entirely in `Engine::create_child` (no policy duplication here).
    pub async fn spawn(
        engine: &Engine,
        parent: &SandboxHandle,
        entrypoint: &str,
        options: SandboxOptions,
        limits: WorkerOptions,
    ) -> Result<Self> {
        validate_options(engine, &limits)?;
        let plugin: PreparedPlugin = parent.prepared_plugin().with_entrypoint(entrypoint)?;
        let child = engine.create_child(parent, plugin, options).await?;
        Ok(Self {
            inner: Arc::new(WorkerInner {
                child,
                slots: Arc::new(Semaphore::new(limits.queue_capacity)),
                bytes: Arc::new(Semaphore::new(limits.max_queued_bytes)),
                options: limits,
                closed: AtomicBool::new(false),
            }),
        })
    }

    /// Send a JSON message and await the worker's reply.
    ///
    /// Queue and byte permits are held for the whole invocation, including the
    /// reply wait (and any cancellation future), so in-flight messages count
    /// against the budgets. Dropping a pending reply future cancels this Worker
    /// and closes admission; use explicit `close()` to await native teardown.
    pub async fn post_message(&self, message: Value) -> Result<Value> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let encoded = serde_json::to_vec(&message).map_err(|err| Error::Execution {
            message: format!("failed to serialize worker message: {err}"),
        })?;
        let size = encoded.len();
        drop(encoded);
        if size > self.inner.options.max_message_bytes {
            return Err(Error::LimitExceeded {
                limit: "max_message_bytes".to_string(),
                detail: format!(
                    "serialized message is {size} bytes, limit is {} bytes",
                    self.inner.options.max_message_bytes
                ),
            });
        }
        let _slot: OwnedSemaphorePermit = match self.inner.slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                return Err(Error::QueueFull {
                    capacity: self.inner.options.queue_capacity,
                })
            }
            Err(tokio::sync::TryAcquireError::Closed) => return Err(Error::Closed),
        };
        let byte_units = size.max(1).min(u32::MAX as usize) as u32;
        let _bytes: OwnedSemaphorePermit =
            match self.inner.bytes.clone().try_acquire_many_owned(byte_units) {
                Ok(permit) => permit,
                Err(tokio::sync::TryAcquireError::NoPermits) => {
                    return Err(Error::LimitExceeded {
                        limit: "max_queued_bytes".to_string(),
                        detail: format!(
                            "message needs {byte_units} bytes but queue budget is exhausted"
                        ),
                    })
                }
                Err(tokio::sync::TryAcquireError::Closed) => return Err(Error::Closed),
            };
        let invocation = InvocationOptions {
            timeout: Some(self.inner.options.invocation_timeout),
            deadline: None,
        };
        let mut pending = PendingMessage {
            inner: self.inner.clone(),
            completed: false,
            _slot,
            _bytes,
        };
        let result = self
            .inner
            .child
            .call("onmessage", message, invocation)
            .await;
        pending.completed = true;
        let result = result?;
        let reply_bytes = serde_json::to_vec(&result)
            .map_err(|error| Error::Execution {
                message: format!("failed to serialize worker reply: {error}"),
            })?
            .len();
        if reply_bytes > self.inner.options.max_message_bytes {
            return Err(Error::LimitExceeded {
                limit: "max_message_bytes".into(),
                detail: format!("worker reply is {reply_bytes} bytes"),
            });
        }
        Ok(result)
    }

    /// Depth of the underlying child sandbox.
    pub fn depth(&self) -> usize {
        self.inner.child.depth()
    }

    /// Borrow the underlying child sandbox handle.
    pub fn sandbox(&self) -> &SandboxHandle {
        &self.inner.child
    }

    /// Cancel the worker: mark closed, close the queues and cancel the child.
    pub fn cancel(&self) {
        self.inner.closed.store(true, Ordering::Release);
        self.inner.slots.close();
        self.inner.bytes.close();
        self.inner.child.cancel();
    }

    /// Close the worker and await child shutdown.
    pub async fn close(&self) -> Result<CloseReport> {
        self.inner.closed.store(true, Ordering::Release);
        self.inner.slots.close();
        self.inner.bytes.close();
        self.inner.child.close().await
    }
}

// Dropping an enqueued reply future must not release admission while its native
// invocation remains live. Cancellation is terminal for this Worker: closing the
// semaphores prevents fresh messages from exploiting the released permits.
struct PendingMessage {
    inner: Arc<WorkerInner>,
    completed: bool,
    _slot: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

impl Drop for PendingMessage {
    fn drop(&mut self) {
        if !self.completed {
            self.inner.closed.store(true, Ordering::Release);
            self.inner.slots.close();
            self.inner.bytes.close();
            self.inner.child.cancel();
        }
    }
}

impl Drop for WorkerInner {
    fn drop(&mut self) {
        // Fallback only: explicit `close` is awaited and joined by the caller.
        self.child.cancel();
    }
}

fn invalid(reason: String) -> Error {
    Error::InvalidConfiguration { reason }
}

fn validate_options(engine: &Engine, limits: &WorkerOptions) -> Result<()> {
    if limits.queue_capacity == 0 || limits.queue_capacity > Semaphore::MAX_PERMITS {
        return Err(invalid("queue_capacity is outside semaphore bounds".into()));
    }
    if limits.max_queued_bytes > Semaphore::MAX_PERMITS {
        return Err(invalid("max_queued_bytes exceeds semaphore bounds".into()));
    }
    if limits.max_queued_bytes == 0 || limits.max_message_bytes == 0 {
        return Err(invalid("message byte limits must be non-zero".into()));
    }
    if limits.max_message_bytes > limits.max_queued_bytes {
        return Err(invalid(
            "max_message_bytes must not exceed max_queued_bytes".into(),
        ));
    }
    if limits.max_queued_bytes > u32::MAX as usize {
        return Err(invalid("max_queued_bytes must not exceed u32::MAX".into()));
    }
    if limits.invocation_timeout < Duration::from_millis(1) {
        return Err(invalid("invocation_timeout must be at least 1ms".into()));
    }
    let engine_limits = engine.limits();
    if limits.invocation_timeout > engine_limits.max_invocation_timeout {
        return Err(invalid(format!(
            "invocation_timeout {:?} exceeds engine max {:?}",
            limits.invocation_timeout, engine_limits.max_invocation_timeout
        )));
    }
    if limits.max_message_bytes > engine_limits.max_message_bytes {
        return Err(invalid(format!(
            "max_message_bytes {} exceeds engine max {}",
            limits.max_message_bytes, engine_limits.max_message_bytes
        )));
    }
    Ok(())
}
