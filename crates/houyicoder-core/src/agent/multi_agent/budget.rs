//! Token budget gate: rejects spawn when the parent's total token
//! spend across all running children would exceed a per-parent cap.
//!
//! The gate is a deny gate (spawn refused, not queued) for budget
//! exhaustion. Concurrency capping (max concurrent children) is a
//! separate concern handled by the spawn path, not here — this gate
//! answers one question: does the parent have budget left for another
//! child?

use std::sync::atomic::{AtomicU64, Ordering};

/// A per-parent token budget gate. Tracks the cumulative token spend
/// across all running children of one parent session. A spawn that
/// would push the total over the cap is refused.
///
/// The cap is a hard upper bound on total child token consumption.
/// It prevents runaway spend: a parent that delegates to 4 children
/// each burning 200k tokens would cost 800k tokens without a gate.
/// The default cap is 500k (enough for 2-3 substantial children, tight
/// enough to catch runaway fan-out early).
pub struct TokenBudget {
    /// Cumulative tokens spent by all running children so far.
    spent: AtomicU64,
    /// The hard cap. Spawns that would exceed this are refused.
    cap: u64,
}

/// The verdict from a budget check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetVerdict {
    /// The spawn is allowed; the projected total is under the cap.
    Allow { projected_total: u64 },
    /// The spawn is refused; the projected total would exceed the cap.
    Exhausted {
        cap: u64,
        spent: u64,
        projected: u64,
    },
}

impl TokenBudget {
    /// Create a budget with the given cap (in tokens).
    pub fn new(cap: u64) -> Self {
        Self {
            spent: AtomicU64::new(0),
            cap,
        }
    }

    /// The default cap: 500k tokens. Enough for 2-3 substantial child
    /// agents, tight enough to catch runaway fan-out.
    pub const DEFAULT_CAP: u64 = 500_000;

    /// Check whether a spawn projecting the given additional token
    /// cost is within budget. Does not deduct — the caller calls
    /// commit only if the spawn actually starts.
    pub fn check(&self, projected_child_cost: u64) -> BudgetVerdict {
        let spent = self.spent.load(Ordering::Relaxed);
        let projected = spent.saturating_add(projected_child_cost);
        if projected > self.cap {
            BudgetVerdict::Exhausted {
                cap: self.cap,
                spent,
                projected,
            }
        } else {
            BudgetVerdict::Allow {
                projected_total: projected,
            }
        }
    }

    /// Commit the projected cost after the spawn actually starts.
    /// Called once per child; the child's actual spend may differ
    /// from the projection, but the gate uses the projection (the
    /// worst case) for the deny decision.
    pub fn commit(&self, projected_child_cost: u64) {
        self.spent
            .fetch_add(projected_child_cost, Ordering::Relaxed);
    }

    /// Release budget when a child completes (its projected cost is
    /// returned to the pool so the parent can spawn another child).
    pub fn release(&self, projected_child_cost: u64) {
        self.spent
            .fetch_sub(projected_child_cost, Ordering::Relaxed);
    }

    /// The cumulative tokens spent so far.
    pub fn spent(&self) -> u64 {
        self.spent.load(Ordering::Relaxed)
    }

    /// The cap.
    pub fn cap(&self) -> u64 {
        self.cap
    }
}

impl Default for TokenBudget {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allow_under_cap() {
        let b = TokenBudget::new(100_000);
        match b.check(40_000) {
            BudgetVerdict::Allow { projected_total } => {
                assert_eq!(projected_total, 40_000);
            }
            _ => panic!("should allow"),
        }
    }

    #[test]
    fn test_refuse_over_cap() {
        let b = TokenBudget::new(100_000);
        b.commit(80_000);
        match b.check(40_000) {
            BudgetVerdict::Exhausted {
                cap,
                spent,
                projected,
            } => {
                assert_eq!(cap, 100_000);
                assert_eq!(spent, 80_000);
                assert_eq!(projected, 120_000);
            }
            _ => panic!("should refuse"),
        }
    }

    #[test]
    fn test_commit_accumulates() {
        let b = TokenBudget::new(500_000);
        b.commit(100_000);
        b.commit(200_000);
        assert_eq!(b.spent(), 300_000);
    }

    #[test]
    fn test_release_returns_budget() {
        let b = TokenBudget::new(100_000);
        b.commit(80_000);
        assert_eq!(b.spent(), 80_000);
        b.release(80_000);
        assert_eq!(b.spent(), 0);
        // After release, a new spawn is within budget again.
        assert!(matches!(b.check(40_000), BudgetVerdict::Allow { .. }));
    }

    /// Check does not mutate: calling check twice with the same
    /// projection returns the same verdict (commit is separate).
    #[test]
    fn test_check_does_not_mutate() {
        let b = TokenBudget::new(100_000);
        let v1 = b.check(40_000);
        let v2 = b.check(40_000);
        assert_eq!(v1, v2);
        assert_eq!(b.spent(), 0);
    }

    /// Saturating add: a huge projection does not overflow.
    #[test]
    fn test_saturating_no_overflow() {
        let b = TokenBudget::new(100_000);
        let huge = u64::MAX;
        match b.check(huge) {
            BudgetVerdict::Exhausted { projected, .. } => {
                assert_eq!(projected, u64::MAX);
            }
            _ => panic!("should refuse"),
        }
    }
}
