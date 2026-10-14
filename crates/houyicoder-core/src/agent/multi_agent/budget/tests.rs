//! Tests for this module.

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
/// forecast returns the same verdict (commit is separate).
#[test]
fn test_check_does_not_mutate() {
    let b = TokenBudget::new(100_000);
    let v1 = b.check(40_000);
    let v2 = b.check(40_000);
    assert_eq!(v1, v2);
    assert_eq!(b.spent(), 0);
}

/// Saturating add: a huge forecast does not overflow.
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
