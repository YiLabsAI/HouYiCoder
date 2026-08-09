//! Review queue + audit trail state for the review-node console. Extracted
//! from App so sign-off / reject / audit concerns stay together. The focus
//! index selects the finding currently in view on the console.

use crate::evidence::{AuditEntry, ReviewFinding, Verdict, audit_entry};

/// The multi-agent review queue: the findings awaiting sign-off, the focused
/// index, and the audit trail of signed-off / rejected findings. All data is
/// stub; no daemon produces it yet.
#[derive(Debug, Clone, Default)]
pub struct ReviewQueue {
    /// Findings awaiting human sign-off, in arrival order.
    pub findings: Vec<ReviewFinding>,
    /// Index into findings for the currently focused console row.
    pub focus: usize,
    /// Append-only audit trail of signed-off / rejected findings.
    pub audit_trail: Vec<AuditEntry>,
}

impl ReviewQueue {
    /// Number of findings in the queue.
    pub fn len(&self) -> usize {
        self.findings.len()
    }

    /// True when the queue holds no findings.
    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }

    /// The focused finding, if any.
    pub fn current(&self) -> Option<&ReviewFinding> {
        self.findings.get(self.focus)
    }

    /// Move the queue focus up, wrapping around.
    pub fn focus_up(&mut self) {
        let n = self.len();
        if n > 0 {
            self.focus = (self.focus + n - 1) % n;
        }
    }

    /// Move the queue focus down, wrapping around.
    pub fn focus_down(&mut self) {
        let n = self.len();
        if n > 0 {
            self.focus = (self.focus + 1) % n;
        }
    }

    /// Sign off on the focused finding: mark signed off and append an audit
    /// trail entry. No-op if the queue is empty or already signed off.
    pub fn signoff_focused(&mut self, who: &str, when: &str) {
        let Some(idx) = self.focus.checked_rem(self.len()) else {
            return;
        };
        if self.findings[idx].signoff == Verdict::Approved {
            return;
        }
        let fid = self.findings[idx].id.clone();
        self.findings[idx].signoff = Verdict::Approved;
        self.audit_trail
            .push(audit_entry(&fid, "signed off", who, when));
    }

    /// Reject the focused finding: mark rejected, write back to org eval
    /// (stub), and append an audit trail entry. Returns the org-eval feedback
    /// note when a rejection happened.
    pub fn reject_focused(&mut self, who: &str, when: &str) -> Option<String> {
        let idx = self.focus.checked_rem(self.len())?;
        if self.findings[idx].signoff == Verdict::Rejected {
            return None;
        }
        let fid = self.findings[idx].id.clone();
        self.findings[idx].signoff = Verdict::Rejected;
        let note = format!(
            "org eval feedback: finding {} is a false positive, down-weight the lens next round (stub)",
            fid
        );
        self.audit_trail
            .push(audit_entry(&fid, "rejected", who, when));
        Some(note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(id: &str) -> ReviewFinding {
        ReviewFinding {
            id: id.to_string(),
            lens: "correctness".to_string(),
            verdict: "real".to_string(),
            severity: "high".to_string(),
            hunk_id: "h".to_string(),
            spec_clause_id: "c".to_string(),
            test_id: "t".to_string(),
            adversarial: String::new(),
            note: String::new(),
            signoff: Verdict::Pending,
        }
    }

    #[test]
    fn test_signoff_appends_audit() {
        let mut q = ReviewQueue {
            findings: vec![finding("f1")],
            focus: 0,
            audit_trail: vec![],
        };
        q.signoff_focused("you", "now");
        assert_eq!(q.findings[0].signoff, Verdict::Approved);
        assert_eq!(q.audit_trail.len(), 1);
    }

    #[test]
    fn test_reject_writes_org_eval() {
        let mut q = ReviewQueue {
            findings: vec![finding("f1")],
            focus: 0,
            audit_trail: vec![],
        };
        let note = q.reject_focused("you", "now");
        assert!(note.is_some());
        assert_eq!(q.findings[0].signoff, Verdict::Rejected);
        assert_eq!(q.audit_trail.len(), 1);
    }

    #[test]
    fn test_signoff_noop_when_empty() {
        let mut q = ReviewQueue::default();
        q.signoff_focused("you", "now");
        assert!(q.audit_trail.is_empty());
    }
}
