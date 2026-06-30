//! Evidence-chain domain types: spec clauses with divergence, per-hunk
//! evidence chains, enriched review findings, and the audit trail. These are
//! wire types — they cross the frontend protocol as payloads of FrontendEvent
//! and FrontendRequest. Pure data plus small domain queries; no UI coupling,
//! so the protocol crate stays free of any rendering dependency.

use super::verdict::Verdict;

/// Spec-vs-impl divergence status for one spec clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divergence {
    /// No hunk implements this clause yet.
    Unimplemented,
    /// A hunk exists but no test covers it.
    Partial,
    /// A hunk exists and a passing test covers it.
    Satisfied,
}

impl Divergence {
    /// One-letter label for the strip chip.
    pub fn label(self) -> &'static str {
        match self {
            Self::Unimplemented => "x",
            Self::Partial => "~",
            Self::Satisfied => "ok",
        }
    }
}

/// One spec clause (acceptance contract line) with its divergence status.
/// Stub data; the real model would point at hunk/test ids covering it.
#[derive(Debug, Clone)]
pub struct SpecClause {
    pub id: String,
    pub text: String,
    pub status: Divergence,
}

/// Evidence chain backing one diff hunk. A hunk is a claim: it cites the spec
/// clause it implements, the review finding that flagged it, and the test that
/// covers it. All ids are stubs.
#[derive(Debug, Clone)]
pub struct HunkEvidence {
    pub spec_clause_id: String,
    pub spec_clause_desc: String,
    pub finding_id: String,
    pub finding_desc: String,
    pub test_id: String,
    pub why: String,
}

/// One diff hunk: a file range plus its evidence chain and approval state.
/// approved is Verdict::Pending while awaiting a verdict, Approved when
/// approved, Rejected when rejected.
#[derive(Debug, Clone)]
pub struct Hunk {
    pub id: String,
    pub file: String,
    pub range: String,
    pub patch: String,
    pub evidence: HunkEvidence,
    pub approved: Verdict,
}

impl Hunk {
    /// True when this hunk has been approved or rejected.
    pub fn resolved(&self) -> bool {
        !matches!(self.approved, Verdict::Pending)
    }

    /// Short label for the current approval state.
    pub fn approval_label(&self) -> &'static str {
        match self.approved {
            Verdict::Pending => "pending",
            Verdict::Approved => "approved",
            Verdict::Rejected => "rejected",
        }
    }
}

/// One review finding from the multi-agent adversarial review. Enriched for
/// the review-node console: a finding carries a verdict (real/refuted),
/// severity, an evidence trail (hunk id + spec clause + test + adversarial
/// summary), and a sign-off state. Sign-off/reject feeds the audit trail;
/// reject also writes back to org eval (stub).
#[derive(Debug, Clone)]
pub struct ReviewFinding {
    pub id: String,
    pub lens: String,
    pub verdict: String,
    pub severity: String,
    pub hunk_id: String,
    pub spec_clause_id: String,
    pub test_id: String,
    pub adversarial: String,
    pub note: String,
    /// Pending / signed off / rejected.
    pub signoff: Verdict,
}

impl ReviewFinding {
    /// True when signed off or rejected.
    pub fn resolved(&self) -> bool {
        !matches!(self.signoff, Verdict::Pending)
    }

    /// Short label for the decision log ("approved" or "rejected").
    pub fn signoff_label(&self) -> &'static str {
        match self.signoff {
            Verdict::Approved => "approved",
            Verdict::Rejected => "rejected",
            Verdict::Pending => "pending",
        }
    }
}

/// One audit-trail entry: a signed-off or rejected finding, with who/when and
/// a replayable reference. A projection of the hash-chain event log. The hash
/// field is a stub hash-chain projection.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub finding_id: String,
    pub verdict: String,
    pub who: String,
    pub when: String,
    pub replay_ref: String,
    pub hash: String,
}

/// Build a stub audit-trail entry with a fake hash-chain projection.
pub fn audit_entry(finding_id: &str, verdict: &str, who: &str, when: &str) -> AuditEntry {
    let replay_ref = format!("replay:{}#{}", finding_id, when);
    let hash = format!("h{:04x}", finding_id.len().wrapping_mul(31) ^ when.len());
    AuditEntry {
        finding_id: finding_id.to_string(),
        verdict: verdict.to_string(),
        who: who.to_string(),
        when: when.to_string(),
        replay_ref,
        hash,
    }
}
