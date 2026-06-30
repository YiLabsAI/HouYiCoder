//! Human verdict on a change, finding, or proposal.
//!
//! A single three-state enum for every human sign-off across the protocol.
//! Replaces the old split between a three-state Decision in the TUI and a
//! two-state SignOffVerdict here: one concept, one type. Pending is a real
//! state (a finding awaits sign-off), so the enum is three-state; sign-off
//! sites that are never pending simply never construct Pending.

use serde::{Deserialize, Serialize};

/// A human verdict: pending while awaiting sign-off, approved, or rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// Awaiting a human verdict.
    Pending,
    /// Approved / signed off.
    Approved,
    /// Rejected.
    Rejected,
}
