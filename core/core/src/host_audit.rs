//! Bounded metadata-only audit records. No payloads, business scope, environment
//! values, or callback error text enter this buffer.
use denosanbox_policy::SandboxId;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    Started,
    Completed,
    Denied,
    Cancelled,
    DeadlineExceeded,
    SchemaRejected,
    LimitExceeded,
    Failed,
}
#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub at: SystemTime,
    pub sandbox: SandboxId,
    /// A registered host-provided capability name, or the fixed `<ungranted>` tag.
    pub capability: String,
    pub outcome: AuditOutcome,
}
#[derive(Debug)]
pub struct HostAudit {
    events: Mutex<VecDeque<AuditEvent>>,
    capacity: usize,
    dropped: AtomicU64,
}
impl Default for HostAudit {
    fn default() -> Self {
        Self::new(256)
    }
}
impl HostAudit {
    pub fn new(capacity: usize) -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            capacity: capacity.min(4096),
            dropped: AtomicU64::new(0),
        }
    }
    pub fn record(&self, mut event: AuditEvent) {
        if event.capability.len() > 64 {
            event.capability = "<invalid>".into();
        }
        let Ok(mut events) = self.events.lock() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if self.capacity == 0 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if events.len() == self.capacity {
            events.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        events.push_back(event);
    }
    pub fn drain(&self, max: usize) -> Vec<AuditEvent> {
        let Ok(mut events) = self.events.lock() else {
            return Vec::new();
        };
        let count = max.min(events.len());
        events.drain(..count).collect()
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn audit_is_bounded_and_tracks_discarded_records() {
        let audit = HostAudit::new(2);
        for _ in 0..5 {
            audit.record(AuditEvent {
                at: SystemTime::now(),
                sandbox: SandboxId::mint(),
                capability: "orders.read".into(),
                outcome: AuditOutcome::Completed,
            });
        }
        assert_eq!(audit.dropped(), 3);
        assert_eq!(audit.drain(10).len(), 2);
        assert!(audit.drain(1).is_empty());
    }
}
