//! Lightweight write-activity counter shared by mutation paths and
//! background schedulers.
//!
//! Each user-facing mutating call on `SemanticStore` / `EpisodicStore`
//! bumps the counter. The L3 consolidation scheduler reads it on tick
//! boundaries and skips a pass when the value changed since the
//! previous tick — i.e. the system is still under load and we'd
//! rather wait for a quiet window before walking thousands of rows.
//!
//! Monotonic by design: counters only grow. The scheduler stores the
//! snapshot it last observed and compares; no resets, no contention
//! with other readers.
//!
//! Internal-only: not exposed via tools, not in `mneme://stats`.
//! Bookkeeping for the scheduler, nothing else.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct ActivityCounter {
    n: AtomicU64,
}

impl ActivityCounter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record one mutation. Cheap; safe from any thread.
    pub fn bump(&self) {
        self.n.fetch_add(1, Ordering::SeqCst);
    }

    /// Current cumulative count. Compare two snapshots taken at
    /// different times to detect activity in the interval.
    pub fn snapshot(&self) -> u64 {
        self.n.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_grows_with_each_bump() {
        let c = ActivityCounter::new();
        assert_eq!(c.snapshot(), 0);
        c.bump();
        c.bump();
        c.bump();
        assert_eq!(c.snapshot(), 3);
    }

    #[test]
    fn cloned_arc_shares_counter() {
        let a = ActivityCounter::new();
        let b = Arc::clone(&a);
        a.bump();
        b.bump();
        assert_eq!(a.snapshot(), 2);
        assert_eq!(b.snapshot(), 2);
    }
}
