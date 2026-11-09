//! Default limits and the narrowing rule between policies.
//!
//! [`Limits`] are *declared* budgets. They bound what the runtime is asked to
//! allow; they are not a hard memory guarantee. In particular
//! `heap_target_bytes` is a target handed to the JS engine's own limiter, not an
//! RSS ceiling — the host must still enforce memory externally (see README).

use std::time::Duration;

/// Default maximum number of live sandboxes per host registry.
pub const DEFAULT_MAX_SANDBOXES: usize = 4;
/// Default maximum queued invocations per sandbox.
pub const DEFAULT_QUEUE_DEPTH: usize = 32;
/// Default wall-clock deadline per invocation.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);
/// Default maximum serialized message size in bytes (1 MiB).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// Default maximum retained log bytes per invocation (64 KiB).
pub const DEFAULT_MAX_LOG_BYTES: usize = 64 * 1024;
/// Default JS heap target in bytes (64 MiB). A target, not a hard limit.
pub const DEFAULT_HEAP_TARGET_BYTES: usize = 64 * 1024 * 1024;

/// A bundle of numeric runtime limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Limits {
    max_sandboxes: usize,
    queue_depth: usize,
    deadline: Duration,
    max_message_bytes: usize,
    max_log_bytes: usize,
    heap_target_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sandboxes: DEFAULT_MAX_SANDBOXES,
            queue_depth: DEFAULT_QUEUE_DEPTH,
            deadline: DEFAULT_DEADLINE,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            max_log_bytes: DEFAULT_MAX_LOG_BYTES,
            heap_target_bytes: DEFAULT_HEAP_TARGET_BYTES,
        }
    }
}

impl Limits {
    /// Maximum live sandboxes.
    #[must_use]
    pub fn max_sandboxes(&self) -> usize {
        self.max_sandboxes
    }
    /// Maximum queued invocations per sandbox.
    #[must_use]
    pub fn queue_depth(&self) -> usize {
        self.queue_depth
    }
    /// Per-invocation wall-clock deadline.
    #[must_use]
    pub fn deadline(&self) -> Duration {
        self.deadline
    }
    /// Maximum serialized message size.
    #[must_use]
    pub fn max_message_bytes(&self) -> usize {
        self.max_message_bytes
    }
    /// Maximum retained log bytes.
    #[must_use]
    pub fn max_log_bytes(&self) -> usize {
        self.max_log_bytes
    }
    /// JS heap *target* (not a hard RSS bound).
    #[must_use]
    pub fn heap_target_bytes(&self) -> usize {
        self.heap_target_bytes
    }

    /// Builder helper.
    #[must_use]
    pub fn with_max_sandboxes(mut self, n: usize) -> Self {
        self.max_sandboxes = n.max(1);
        self
    }
    /// Builder helper.
    #[must_use]
    pub fn with_queue_depth(mut self, n: usize) -> Self {
        self.queue_depth = n.max(1);
        self
    }
    /// Builder helper.
    #[must_use]
    pub fn with_deadline(mut self, d: Duration) -> Self {
        self.deadline = d;
        self
    }
    /// Builder helper.
    #[must_use]
    pub fn with_max_message_bytes(mut self, n: usize) -> Self {
        self.max_message_bytes = n.max(1);
        self
    }
    /// Builder helper.
    #[must_use]
    pub fn with_max_log_bytes(mut self, n: usize) -> Self {
        self.max_log_bytes = n.max(1);
        self
    }
    /// Builder helper.
    #[must_use]
    pub fn with_heap_target_bytes(mut self, n: usize) -> Self {
        self.heap_target_bytes = n.max(1);
        self
    }

    /// Per-field minimum: the narrowing operator used when deriving children.
    /// Every field becomes the stricter of the two, never the looser one.
    #[must_use]
    pub fn narrow(self, other: Limits) -> Limits {
        Limits {
            max_sandboxes: self.max_sandboxes.min(other.max_sandboxes),
            queue_depth: self.queue_depth.min(other.queue_depth),
            deadline: self.deadline.min(other.deadline),
            max_message_bytes: self.max_message_bytes.min(other.max_message_bytes),
            max_log_bytes: self.max_log_bytes.min(other.max_log_bytes),
            heap_target_bytes: self.heap_target_bytes.min(other.heap_target_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_values() {
        let l = Limits::default();
        assert_eq!(l.max_sandboxes(), 4);
        assert_eq!(l.queue_depth(), 32);
        assert_eq!(l.deadline(), Duration::from_secs(30));
        assert_eq!(l.max_message_bytes(), 1024 * 1024);
        assert_eq!(l.max_log_bytes(), 64 * 1024);
        assert_eq!(l.heap_target_bytes(), 64 * 1024 * 1024);
    }

    #[test]
    fn narrow_takes_stricter_field_each_time() {
        let a = Limits::default();
        let b = Limits::default()
            .with_max_sandboxes(2)
            .with_deadline(Duration::from_secs(5));
        let n = a.narrow(b);
        assert_eq!(n.max_sandboxes(), 2);
        assert_eq!(n.deadline(), Duration::from_secs(5));
        assert_eq!(n.max_message_bytes(), a.max_message_bytes());
        assert_eq!(n.narrow(a), n);
    }
}
