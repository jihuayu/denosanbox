//! Cancellation, deadlines and hierarchical byte budgets.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// Cooperative cancellation flag shared by host and guest.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Fresh, uncancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Request cancellation. Idempotent and safe from any thread.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    /// Error unless cancellation was requested.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::cancelled("invocation cancelled"))
        } else {
            Ok(())
        }
    }
}

/// Absolute deadline for one invocation.
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    at: Instant,
}

impl Deadline {
    /// Deadline `after` from now.
    #[must_use]
    pub fn after(after: Duration) -> Self {
        let now = Instant::now();
        Self {
            at: now.checked_add(after).unwrap_or(now),
        }
    }
    /// Keep the earlier absolute deadline, so descendants cannot extend it.
    pub fn earliest(self, other: Self) -> Self {
        Self {
            at: self.at.min(other.at),
        }
    }
    pub fn child_after(self, duration: Duration) -> Self {
        self.earliest(Self::after(duration))
    }
    /// Whether the deadline has passed.
    #[must_use]
    pub fn expired(&self) -> bool {
        Instant::now() >= self.at
    }
    /// Remaining time, or `None` once expired.
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        self.at.checked_duration_since(Instant::now())
    }
    /// Error once expired.
    pub fn check(&self) -> Result<()> {
        if self.expired() {
            Err(Error::deadline_exceeded("deadline exceeded"))
        } else {
            Ok(())
        }
    }
}

enum Charge {
    Ok,
    Limit,
    Overflow,
}

fn try_charge(counter: &AtomicU64, amount: u64, limit: u64) -> Charge {
    let mut cur = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = cur.checked_add(amount) else {
            return Charge::Overflow;
        };
        if next > limit {
            return Charge::Limit;
        }
        match counter.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Charge::Ok,
            Err(actual) => cur = actual,
        }
    }
}

/// A hierarchical byte budget. Children charge themselves *and* every
/// ancestor, so a subtree can never exceed its own limit or any enclosing one.
#[derive(Debug)]
pub struct Budget {
    limit: u64,
    used: Arc<AtomicU64>,
    parent: Option<Arc<Budget>>,
}

impl Budget {
    /// Root budget with `limit` bytes.
    #[must_use]
    pub fn new(limit: u64) -> Arc<Budget> {
        Arc::new(Budget {
            limit,
            used: Arc::new(AtomicU64::new(0)),
            parent: None,
        })
    }

    /// Child budget; reserves also charge this budget's ancestors.
    #[must_use]
    pub fn child(self: &Arc<Self>, limit: u64) -> Arc<Budget> {
        Arc::new(Budget {
            limit,
            used: Arc::new(AtomicU64::new(0)),
            parent: Some(Arc::clone(self)),
        })
    }

    /// This budget's own limit.
    #[must_use]
    pub fn limit(&self) -> u64 {
        self.limit
    }
    /// This budget's own reserved bytes (excluding ancestors).
    #[must_use]
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }
    /// Remaining bytes under this node's own limit.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used())
    }

    /// Reserve `amount` bytes across this budget and all ancestors. The
    /// returned [`Lease`] releases the reservation on drop.
    pub fn reserve(&self, amount: u64) -> Result<Lease> {
        let mut chain: Vec<&Budget> = Vec::new();
        let mut cursor = Some(self);
        while let Some(b) = cursor {
            chain.push(b);
            cursor = b.parent.as_deref();
        }
        // Charge ancestors first so a rejected descendant charge never leaves a
        // partial reservation behind on a lower node.
        chain.reverse();
        let mut lease = Lease {
            charges: Vec::with_capacity(chain.len()),
        };
        for b in chain {
            match try_charge(&b.used, amount, b.limit) {
                Charge::Ok => lease.charges.push((Arc::clone(&b.used), amount)),
                Charge::Limit => {
                    return Err(Error::limit_exceeded("budget limit exceeded"));
                }
                Charge::Overflow => return Err(Error::overflow("budget accounting overflow")),
            }
        }
        Ok(lease)
    }
}

/// Owned reservation; releases every node on drop.
#[derive(Debug)]
pub struct Lease {
    charges: Vec<(Arc<AtomicU64>, u64)>,
}

impl Lease {
    /// Total amount reserved.
    #[must_use]
    pub fn amount(&self) -> u64 {
        self.charges.first().map_or(0, |(_, amount)| *amount)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        for (counter, amount) in self.charges.drain(..) {
            counter.fetch_sub(amount, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestor_accounting_and_drop_release() {
        let root = Budget::new(100);
        let mid = root.child(80);
        let leaf = mid.child(50);
        let l = leaf.reserve(30).unwrap();
        assert_eq!(root.used(), 30);
        assert_eq!(mid.used(), 30);
        assert_eq!(leaf.used(), 30);
        assert!(leaf.reserve(21).is_err());
        drop(l);
        assert_eq!(root.used(), 0);
        assert_eq!(leaf.remaining(), 50);
    }

    #[test]
    fn parent_limit_stops_child() {
        let root = Budget::new(10);
        let leaf = root.child(1000);
        assert!(leaf.reserve(11).is_err());
        let _l = leaf.reserve(10).unwrap();
        assert!(root.reserve(1).is_err());
    }

    #[test]
    fn overflow_is_detected() {
        let b = Budget::new(u64::MAX);
        let _l = b.reserve(u64::MAX).unwrap();
        let e = b.reserve(1).unwrap_err();
        assert_eq!(e.kind(), crate::ErrorKind::Overflow);
    }

    #[test]
    fn cancellation_and_deadline() {
        let t = CancellationToken::new();
        t.check().unwrap();
        t.cancel();
        assert_eq!(t.check().unwrap_err().kind(), crate::ErrorKind::Cancelled);
        let d = Deadline::after(Duration::from_millis(20));
        d.check().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert!(d.expired());
        assert_eq!(
            d.check().unwrap_err().kind(),
            crate::ErrorKind::DeadlineExceeded
        );
    }
}
