//! Per-pane approval state machine. Approving, rejecting, and reworking move
//! the spec-to-signoff chain, and each pane decides what its own verdict
//! means. Focused navigation inside those panes lives here too, because the
//! key that walks a hunk list and the key that approves one answer the same
//! pane.

use crate::evidence::{Divergence, Verdict};
use crate::state::{App, Pane, Stage};

impl App {
    /// Approve the artifact/hunk/finding/proposal for the current pane. Each
    /// pane knows what approval means: spec/plan approve the design; diff
    /// approves a hunk; review signs off a finding; verify completes the chain;
    /// artifact applies the pending proposed edit (stage-independent).
    pub(crate) fn approve_in_pane(&mut self) {
        let stage = self.workflow.stage;
        match self.ui.navigation.pane {
            Pane::Spec | Pane::Plan if stage == Stage::Design => self.approve_design(),
            Pane::Diff if stage == Stage::Implementing => self.approve_focused_hunk(),
            Pane::Review if stage == Stage::Verify => self.signoff_focused_finding(),
            Pane::Verify if stage == Stage::Verify => self.complete_verify(),
            Pane::Artifact => {
                self.workflow.artifact.approve_pending();
                self.system_line("artifact: change applied (append-only audit updated)");
            }
            _ => {}
        }
    }

    /// Reject the focused hunk/finding/proposal for the current pane. The
    /// artifact pane rejects the pending proposed edit (stage-independent).
    pub(crate) fn reject_in_pane(&mut self) {
        match self.ui.navigation.pane {
            Pane::Diff if self.workflow.stage == Stage::Implementing => self.reject_focused_hunk(),
            Pane::Review if self.workflow.stage == Stage::Verify => self.reject_focused_finding(),
            Pane::Artifact => {
                self.workflow.artifact.reject_pending();
                self.system_line("artifact: proposed edit rejected");
            }
            _ => {}
        }
    }

    /// Move focus up/down within the current pane (hunks in diff, findings in
    /// review, clauses in spec, lines in artifact). No-op on panes without a
    /// focusable list.
    pub(crate) fn navigate_pane(&mut self, down: bool) {
        match self.ui.navigation.pane {
            Pane::Diff => {
                if down {
                    self.workflow.diff.focus_down();
                } else {
                    self.workflow.diff.focus_up();
                }
            }
            Pane::Review => {
                if down {
                    self.workflow.review.focus_down();
                } else {
                    self.workflow.review.focus_up();
                }
            }
            Pane::Spec => {
                self.workflow.spec_ctx.clause_focus = {
                    let n = self.workflow.spec_clauses.len();
                    if n == 0 {
                        0
                    } else if down {
                        (self.workflow.spec_ctx.clause_focus + 1) % n
                    } else {
                        (self.workflow.spec_ctx.clause_focus + n - 1) % n
                    }
                }
            }
            Pane::Artifact => {
                if down {
                    self.workflow.artifact.focus_down();
                } else {
                    self.workflow.artifact.focus_up();
                }
            }
            _ => {}
        }
    }

    /// Approve the design (one approval covering spec + plan). Both artifacts
    /// are marked approved and the chain advances to implementing.
    fn approve_design(&mut self) {
        if self.workflow.spec_artifact.approved && self.workflow.plan_artifact.approved {
            return;
        }
        self.workflow.spec_artifact.approved = true;
        self.workflow.plan_artifact.approved = true;
        self.enter_stage(
            Stage::Implementing,
            Pane::Diff,
            "design approved -> implementing",
        );
    }

    fn approve_focused_hunk(&mut self) {
        if !self.workflow.diff.approve_focused() {
            return;
        }
        let hunk = self.workflow.diff.current();
        let clause_id = hunk
            .map(|h| h.evidence.spec_clause_id.clone())
            .unwrap_or_default();
        // A change now implements the requirement: unimpl -> partial.
        self.set_clause_status(&clause_id, Divergence::Partial);
        self.system_line("change approved -> spec requirement moved to partial");
        if self.all_hunks_approved() {
            // All changes approved -> verify stage, agent-review phase first.
            self.enter_stage(
                Stage::Verify,
                Pane::Review,
                "all changes approved -> verify (agent review)",
            );
        } else {
            // Auto-advance focus to the next pending change so a repeated a
            // key approves each change in turn, then trips the all-approved
            // transition. Without this the flow stalls on the just-approved
            // change (a on an approved change is a no-op).
            self.workflow.diff.focus_next_pending();
        }
    }

    fn reject_focused_hunk(&mut self) {
        if !self.workflow.diff.reject_focused() {
            return;
        }
        self.system_line("change rejected (requirement status unchanged)");
    }

    fn signoff_focused_finding(&mut self) {
        let before = self.workflow.review.audit_trail.len();
        self.signoff_focused("you", "now");
        if self.workflow.review.audit_trail.len() > before {
            self.system_line("finding approved -> decision log grew");
        }
        if self.all_findings_resolved() {
            // Agent review done -> machine-check phase (same verify stage).
            self.ui.navigation.pane = Pane::Verify;
            self.system_line("agent review done -> machine check");
        }
    }

    fn reject_focused_finding(&mut self) {
        if let Some(note) = self.reject_focused("you", "now") {
            self.system_line(note);
        }
        if self.all_findings_resolved() {
            self.ui.navigation.pane = Pane::Verify;
            self.system_line("agent review done -> machine check");
        }
    }

    fn complete_verify(&mut self) {
        // Verify passes only when every check is green. A failed-checks
        // state blocks completion and routes the user to the rework path
        // (r) instead.
        if !self.workflow.verify_result.passed {
            self.system_line("verify: checks failed -- press r to rework (stub)");
            return;
        }
        // Verify passes: every requirement with an approved change is satisfied.
        // Collect ids first to avoid borrowing self.workflow.diff while mutating clauses.
        let diff = &self.workflow.diff;
        let approved_clauses: Vec<String> = diff
            .hunks
            .iter()
            .filter(|h| h.approved == Verdict::Approved)
            .map(|h| h.evidence.spec_clause_id.clone())
            .collect();
        for cid in approved_clauses {
            self.set_clause_status(&cid, Divergence::Satisfied);
        }
        self.enter_stage(
            Stage::Done,
            Pane::Verify,
            "verify passed -> done (all checks green)",
        );
    }

    /// Rework backward path: from the review phase of verify, pressing i on a
    /// real finding sends the chain back to implementing and regresses that
    /// finding's spec requirement to partial. From the machine-check phase,
    /// pressing r on failed checks sends the chain back to implementing.
    pub(crate) fn rework_in_pane(&mut self) {
        match self.ui.navigation.pane {
            Pane::Review if self.workflow.stage == Stage::Verify => self.rework_from_review(),
            Pane::Verify if self.workflow.stage == Stage::Verify => self.rework_from_verify(),
            _ => {}
        }
    }

    fn rework_from_review(&mut self) {
        let Some(f) = self.workflow.review.current() else {
            return;
        };
        if f.verdict != "real" {
            self.system_line("rework: only real findings trigger a rework (i)");
            return;
        }
        let clause_id = f.spec_clause_id.clone();
        let finding_id = f.id.clone();
        // Regress the requirement: a real finding means the change is incomplete.
        self.set_clause_status(&clause_id, Divergence::Partial);
        self.enter_stage(
            Stage::Implementing,
            Pane::Diff,
            &format!("rework: finding {finding_id} is real -> back to implementing"),
        );
    }

    fn rework_from_verify(&mut self) {
        self.enter_stage(
            Stage::Implementing,
            Pane::Diff,
            "verify rework -> back to implementing",
        );
    }
}
