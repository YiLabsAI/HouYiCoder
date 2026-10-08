//! Guided-chain review and approval state on App: resetting the chain to a
//! fresh draft, moving a clause's divergence status, and the all-hunks-
//! approved gate. Split from App so the chain's working state is one cohesive
//! unit rather than scattered App methods.

use crate::state::{App, Divergence, Verdict};

impl App {
    /// Reset the guided-chain working state to a fresh draft: hunks back to
    /// Pending, findings back to Pending, audit trail cleared, clause statuses
    /// reset, artifact approvals cleared, history cleared, replaying off.
    pub fn reset_chain_state(&mut self) {
        for h in &mut self.diff.hunks {
            h.approved = Verdict::Pending;
        }
        self.diff.focus = 0;
        for f in &mut self.review.findings {
            f.signoff = Verdict::Pending;
        }
        self.review.focus = 0;
        self.review.audit_trail.clear();
        for c in &mut self.spec_clauses {
            c.status = Divergence::Unimplemented;
        }
        self.spec_artifact.approved = false;
        self.plan_artifact.approved = false;
        self.stage_history.clear();
        self.replaying = false;
    }

    /// Move a spec clause to a new divergence status by clause id. No-op when
    /// the clause id is not found.
    pub fn set_clause_status(&mut self, clause_id: &str, status: Divergence) {
        if let Some(c) = self.spec_clauses.iter_mut().find(|c| c.id == clause_id) {
            c.status = status;
        }
    }

    /// True when every diff hunk has been approved (none still pending or
    /// rejected). Used to auto-advance from Implementing to Verify (review
    /// pane).
    pub fn all_hunks_approved(&self) -> bool {
        !self.diff.hunks.is_empty()
            && self
                .diff
                .hunks
                .iter()
                .all(|h| h.approved == Verdict::Approved)
    }
}
