//! Data-record types for the evidence and audit prototype that stay in the
//! TUI crate: the diff data (per-hunk focus plus approve/reject) and the stub
//! result and artifact records for each capability pane. The pure-domain
//! evidence types (Divergence, SpecClause, Hunk, HunkEvidence, ReviewFinding,
//! AuditEntry, Verdict, audit_entry) live in the protocol's frontend types
//! and are re-exported here so existing crate-internal paths keep resolving.

pub use houyicoder_protocol::frontend::{
    AuditEntry, Divergence, Hunk, HunkEvidence, ReviewFinding, SpecClause, Verdict, audit_entry,
};

/// Side-by-side diff data: the list of hunks (each a claim with evidence) and
/// which hunk is currently focused for per-hunk approval.
#[derive(Debug, Clone)]
pub struct DiffData {
    pub path: String,
    pub hunks: Vec<Hunk>,
    pub focus: usize,
}

impl DiffData {
    /// The currently focused hunk, if any.
    pub fn current(&self) -> Option<&Hunk> {
        self.hunks.get(self.focus)
    }

    /// Move focus up, wrapping around.
    pub fn focus_up(&mut self) {
        if self.hunks.is_empty() {
            return;
        }
        self.focus = (self.focus + self.hunks.len() - 1) % self.hunks.len();
    }

    /// Move focus down, wrapping around.
    pub fn focus_down(&mut self) {
        if self.hunks.is_empty() {
            return;
        }
        self.focus = (self.focus + 1) % self.hunks.len();
    }

    /// Approve the focused hunk (stub: sets approved). Returns true on change.
    pub fn approve_focused(&mut self) -> bool {
        let Some(h) = self.hunks.get_mut(self.focus) else {
            return false;
        };
        if h.approved == Verdict::Approved {
            return false;
        }
        h.approved = Verdict::Approved;
        true
    }

    /// Reject the focused hunk (stub: sets rejected). Returns true on change.
    pub fn reject_focused(&mut self) -> bool {
        let Some(h) = self.hunks.get_mut(self.focus) else {
            return false;
        };
        if h.approved == Verdict::Rejected {
            return false;
        }
        h.approved = Verdict::Rejected;
        true
    }

    /// Advance focus to the next still-pending hunk, searching forward and
    /// wrapping. Used after a successful approve so that pressing a repeatedly
    /// walks through every pending change in order instead of sticking on the
    /// just-approved one. No-op when every hunk is already resolved (the
    /// caller handles the all-approved stage transition).
    pub fn focus_next_pending(&mut self) {
        let n = self.hunks.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let idx = (self.focus + step) % n;
            if self.hunks[idx].approved == Verdict::Pending {
                self.focus = idx;
                return;
            }
        }
    }
}

/// A spec artifact from the guided design flow.
#[derive(Debug, Clone)]
pub struct SpecArtifact {
    pub id: String,
    pub title: String,
    pub acceptance: Vec<String>,
    pub contract: Vec<String>,
    pub test_plan: Vec<String>,
    pub approved: bool,
}

/// A plan artifact from the guided plan flow.
#[derive(Debug, Clone)]
pub struct PlanArtifact {
    pub id: String,
    pub steps: Vec<String>,
    pub approved: bool,
}

/// Verify result from Z3 / tests / eval.
#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub checks: Vec<String>,
    pub passed: bool,
}

/// A code graph query result (impact set).
#[derive(Debug, Clone)]
pub struct GraphResult {
    pub query: String,
    pub impact: Vec<String>,
}

/// A memory entry shown in the memory pane.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub topic: String,
    pub summary: String,
    /// Lowercase storage scope label (user / project / auto) — which root
    /// the topic lives in. Drives the pane scope filter.
    pub scope: String,
    /// Lowercase provenance source label (user / feedback / project /
    /// reference) — what the memory is. Shown as a per-row tag.
    pub source: String,
}

/// One agent in the multi-agent fleet.
#[derive(Debug, Clone)]
pub struct AgentStatus {
    pub name: String,
    pub role: String,
    pub state: String,
}

/// One console dashboard todo (PR / MR / issue / CI stub). Kept for the slim
/// work-inbox column on the console screen.
#[derive(Debug, Clone)]
pub struct ConsoleTodo {
    pub kind: String,
    pub title: String,
    pub state: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_focus_wrap_empty() {
        let mut d = DiffData {
            path: "x".into(),
            hunks: Vec::new(),
            focus: 0,
        };
        d.focus_down();
        assert_eq!(d.focus, 0);
    }

    #[test]
    fn test_diff_approve_reject() {
        let mut d = DiffData {
            path: "x".into(),
            hunks: vec![Hunk {
                id: "h1".into(),
                file: "f".into(),
                range: "1-2".into(),
                patch: "".into(),
                evidence: HunkEvidence {
                    spec_clause_id: "c1".into(),
                    spec_clause_desc: "".into(),
                    finding_id: "".into(),
                    finding_desc: "".into(),
                    test_id: "".into(),
                    why: "".into(),
                },
                approved: Verdict::Pending,
            }],
            focus: 0,
        };
        assert!(d.approve_focused());
        assert_eq!(d.current().unwrap().approved, Verdict::Approved);
        assert!(!d.approve_focused());
        assert!(d.reject_focused());
        assert_eq!(d.current().unwrap().approval_label(), "rejected");
    }
}
