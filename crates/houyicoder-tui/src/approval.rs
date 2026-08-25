//! Guided-chain approval state machine and the artifact closed-loop actions,
//! as a second impl App block (Rust permits impl across files). Kept out of
//! command.rs so command.rs owns command dispatch and input parsing only
//! (single responsibility), while this file owns per-pane approve/reject and
//! the modal edit -> proposed change -> apply orchestration.

use crate::state::{App, ArtifactMode, ChangeProposer, Pane, Stage};

impl App {
    /// Approve the artifact/hunk/finding/proposal for the current pane. Each
    /// pane knows what approval means: spec/plan approve the design; diff
    /// approves a hunk; review signs off a finding; verify completes the chain;
    /// artifact applies the pending proposed edit (stage-independent).
    pub(crate) fn approve_in_pane(&mut self) {
        match self.pane {
            Pane::Spec | Pane::Plan if self.stage == Stage::Design => self.approve_design(),
            Pane::Diff if self.stage == Stage::Implementing => self.approve_focused_hunk(),
            Pane::Review if self.stage == Stage::Verify => self.signoff_focused_finding(),
            Pane::Verify if self.stage == Stage::Verify => self.complete_verify(),
            Pane::Artifact => {
                self.artifact.approve_pending();
                self.system_line("artifact: change applied (append-only audit updated)");
            }
            _ => {}
        }
    }

    /// Reject the focused hunk/finding/proposal for the current pane. The
    /// artifact pane rejects the pending proposed edit (stage-independent).
    pub(crate) fn reject_in_pane(&mut self) {
        match self.pane {
            Pane::Diff if self.stage == Stage::Implementing => self.reject_focused_hunk(),
            Pane::Review if self.stage == Stage::Verify => self.reject_focused_finding(),
            Pane::Artifact => {
                self.artifact.reject_pending();
                self.system_line("artifact: proposed edit rejected");
            }
            _ => {}
        }
    }

    /// The visible-row index (into the per-row metadata vecs published by
    /// the last draw) of the selection anchor, mapping its content row
    /// through the current scroll offset. None when no anchor is set or the
    /// anchor row is scrolled out of the viewport.
    pub(crate) fn anchor_visible_row(&self) -> Option<usize> {
        let (_, content_row) = self.selection.anchor?;
        let total = self.transcript_scroll.total.get();
        let top = self.transcript_scroll.top_offset(total);
        let ri = content_row.checked_sub(top)?;
        (ri < self.transcript_rect.get().height as usize).then_some(ri)
    }

    /// Toggle expand/collapse of a tool result row (Ctrl+O). If the selection
    /// anchor sits on a result row, toggle that one; otherwise toggle the last
    /// result in the transcript. No-op when no result is present. Keyed by
    /// call_id so the choice survives the wholesale transcript rebuild on each
    /// event batch.
    pub(crate) fn toggle_focused_result_expand(&mut self) {
        let target = self
            .anchor_visible_row()
            .and_then(|ri| self.last_row_callids.borrow().get(ri).cloned().flatten())
            .or_else(|| {
                self.transcript.iter().rev().find_map(|l| match l {
                    crate::records::TranscriptLine::Tool { name, call_id, .. }
                        if name.as_str() == "result" =>
                    {
                        Some(call_id.clone())
                    }
                    _ => None,
                })
            });
        if let Some(id) = target {
            // remove returns true if it was present; insert only when it was
            // not, giving a clean toggle in one pass.
            if !self.expanded_results.remove(&id) {
                self.expanded_results.insert(id);
            }
        }
    }

    /// Toggle expand/collapse of the fold group under the selection anchor
    /// (Ctrl+O on a summary or collapse-hint row). If the anchor row carries
    /// a fold-group key, toggle that group in expanded_fold_groups. No-op
    /// when the anchor is not on a fold row.
    pub(crate) fn toggle_focused_fold_expand(&mut self) {
        let key = self
            .anchor_visible_row()
            .and_then(|ri| self.last_row_fold_keys.borrow().get(ri).cloned().flatten());
        if let Some(key) = key
            && !self.expanded_fold_groups.remove(&key)
        {
            self.expanded_fold_groups.insert(key);
        }
    }

    /// Toggle a Subagent delegation's inline expansion (Ctrl+O). If the
    /// cursor is on a Subagent line, toggle that one; otherwise fall back to
    /// the most recent. On first expand of a line whose child rows are not
    /// yet loaded, fires a one-shot fetch; a re-expand reuses the cached
    /// rows.
    pub(crate) fn toggle_subagent_expand(&mut self) -> bool {
        let Some((child_sid, needs_fetch)) = self.subagent_target_at_cursor() else {
            return false;
        };
        let expanding = !self.expanded_subagents.contains(&child_sid);
        if expanding {
            self.expanded_subagents.insert(child_sid.clone());
        } else {
            self.expanded_subagents.remove(&child_sid);
        }
        self.transcript_scroll.follow_tail = false;
        // First expand of an unloaded line fires the fetch. The placeholder
        // renders until the snapshot returns; with no backend wired the
        // placeholder stays, matching the other on-demand queries.
        if expanding
            && needs_fetch
            && let Some(req_id) = self.mint_request_id()
        {
            self.send_cmd(crate::run_control::ClientCommand::ChildTranscriptQuery {
                req_id,
                child_sid: houyicoder_protocol::frontend::SessionId(child_sid),
            });
        }
        true
    }

    /// Resolve the Subagent delegation under the selection cursor, or fall
    /// back to the most recent when no cursor is set. Returns the child
    /// session id and whether the child transcript still needs fetching.
    /// Shared by inline expand (Ctrl+O) and teammate-view entry (Enter) so
    /// the line targeted for one is the line drilled into by the other.
    pub(crate) fn subagent_target_at_cursor(&self) -> Option<(String, bool)> {
        use crate::records::TranscriptLine;
        self.selection
            .anchor
            .and_then(|(_, content_row)| {
                let mut row = 0usize;
                let mut first = true;
                for line in &self.transcript {
                    if !first && !matches!(line, TranscriptLine::Interrupted) {
                        row += 1;
                    }
                    let n = self.line_display_rows(line);
                    if row + n > content_row {
                        if let TranscriptLine::Subagent {
                            child_sid,
                            folded_transcript,
                            ..
                        } = line
                        {
                            return Some((child_sid.clone(), folded_transcript.is_empty()));
                        }
                        return None;
                    }
                    row += n;
                    first = false;
                }
                None
            })
            .or_else(|| {
                self.transcript.iter().rev().find_map(|line| match line {
                    TranscriptLine::Subagent {
                        child_sid,
                        folded_transcript,
                        ..
                    } => Some((child_sid.clone(), folded_transcript.is_empty())),
                    _ => None,
                })
            })
    }

    /// Toggle the LAST ThoughtFor line's inline reasoning expansion (Ctrl+O).
    /// Targets the most recent "Thought for Ns" line (near the tail) — no
    /// ambiguity for keyboard users (the latest is unique). For older
    /// ThoughtFor lines the user scrolls + clicks. Returns true when a
    /// ThoughtFor with reasoning was toggled.
    pub(crate) fn toggle_thinking_expand(&mut self) -> bool {
        use crate::records::TranscriptLine;
        // Scan the transcript in reverse for the last ThoughtFor with
        // reasoning. The latest is unique → no ambiguity for keyboard users.
        // Key by turn_id (not reasoning text) so two turns with identical
        // reasoning do not collide.
        for line in self.transcript.iter().rev() {
            if let TranscriptLine::ThoughtFor {
                reasoning: Some(_),
                turn_id,
                ..
            } = line
            {
                if !self.expanded_thinking.remove(turn_id) {
                    self.expanded_thinking.insert(turn_id.clone());
                }
                self.transcript_scroll.follow_tail = false;
                return true;
            }
        }
        false
    }

    /// Toggle thinking expansion by display row index (used by the click
    /// handler). Click on a "Thought for Ns" row toggles inline reasoning
    /// expand/collapse — direct target, no cursor drift (the ThoughtFor row
    /// stays at its screen position; expansion adds rows BELOW). Returns true
    /// when the row is a ThoughtFor row with reasoning.
    pub(crate) fn toggle_thinking_expand_at_row(&mut self, ri: usize) -> bool {
        use crate::records::TranscriptLine;
        // Resolve the click row straight to the ThoughtFor turn_id published
        // by the draw pass (parallel to last_row_fold_keys). The old path
        // counted "Nth visible expandable ThoughtFor" then matched it to
        // "Nth in the FULL transcript" — when ThoughtFor rows scrolled out
        // of the viewport the two counts diverged (the visible count skipped
        // off-screen rows, the full count did not), so clicking a visible
        // row toggled an off-screen turn ("point at bottom, expand at top").
        let turn_id = self.last_row_turn_ids.borrow().get(ri).cloned().flatten();
        let Some(turn_id) = turn_id else {
            return false;
        };
        // Only a ThoughtFor carrying reasoning is expandable. The published
        // turn_id is set only on the header row; confirm the transcript line
        // is a reasoning-bearing ThoughtFor before toggling (defensive — the
        // render path already gates the hint on reasoning: Some).
        let has_reasoning = self
            .transcript
            .iter()
            .any(|line| matches!(line, TranscriptLine::ThoughtFor { reasoning: Some(_), turn_id: tid, .. } if tid == &turn_id));
        if !has_reasoning {
            return false;
        }
        if !self.expanded_thinking.remove(&turn_id) {
            self.expanded_thinking.insert(turn_id.clone());
        }
        self.transcript_scroll.follow_tail = false;
        true
    }

    /// Toggle expand/collapse of a fold group at a display row index (used by
    /// the click handler, which knows the row from the mouse y coordinate).
    /// No-op when the row is not a fold row.
    pub(crate) fn toggle_fold_at_row(&mut self, ri: usize) {
        let key = self.last_row_fold_keys.borrow().get(ri).cloned().flatten();
        if let Some(key) = key
            && !self.expanded_fold_groups.remove(&key)
        {
            self.expanded_fold_groups.insert(key);
        }
    }

    /// Collapse the expanded fold group under the selection anchor, if the
    /// anchor row sits inside an expanded block. Used by the mouse-up handler:
    /// a clean click (no drag motion, no word/line span) anywhere in an
    /// expanded block collapses that block. Returns true when a group was
    /// collapsed (the caller skips the drag-finish/copy path); false otherwise
    /// (the caller falls through to finish_drag). Drag-select inside an
    /// expanded block is preserved because a real drag never reaches here
    /// (is_click_only gates the call site).
    pub(crate) fn collapse_expanded_under_anchor(&mut self) -> bool {
        let Some(ri) = self.anchor_visible_row() else {
            return false;
        };
        let key = self
            .last_row_expanded_group
            .borrow()
            .get(ri)
            .and_then(|f| f.clone())
            .filter(|k| self.expanded_fold_groups.contains(k));
        let Some(key) = key else {
            return false;
        };
        self.expanded_fold_groups.remove(&key);
        self.selection.clear();
        true
    }

    /// Move focus up/down within the current pane (hunks in diff, findings in
    /// review, clauses in spec, lines in artifact). No-op on panes without a
    /// focusable list.
    pub(crate) fn navigate_pane(&mut self, down: bool) {
        match self.pane {
            Pane::Diff => {
                if down {
                    self.diff.focus_down();
                } else {
                    self.diff.focus_up();
                }
            }
            Pane::Review => {
                if down {
                    self.review.focus_down();
                } else {
                    self.review.focus_up();
                }
            }
            Pane::Spec => {
                self.spec_ctx.clause_focus = {
                    let n = self.spec_clauses.len();
                    if n == 0 {
                        0
                    } else if down {
                        (self.spec_ctx.clause_focus + 1) % n
                    } else {
                        (self.spec_ctx.clause_focus + n - 1) % n
                    }
                }
            }
            Pane::Artifact => {
                if down {
                    self.artifact.focus_down();
                } else {
                    self.artifact.focus_up();
                }
            }
            _ => {}
        }
    }

    /// Submit the in-progress artifact edit on Enter. Dispatches by the
    /// session's edit mode: Replace and Insert build a pending proposal
    /// directly from the typed text and the focused line (no proposer);
    /// NaturalLanguage attaches the text as an annotation and asks the
    /// proposer (the LLM seam) to derive an edit. The mode returns to Normal
    /// after submit. Empty text is a no-op (the mode is preserved so the user
    /// can type and resubmit, or Esc to cancel).
    pub(crate) fn artifact_submit_edit(&mut self, text: String) {
        let line = self.artifact.focus() + 1;
        match self.artifact.mode() {
            ArtifactMode::Replace => {
                if text.trim().is_empty() {
                    self.input.set(text);
                    return;
                }
                self.artifact.set_pending_replace(text);
                self.artifact.cancel_mode();
                self.system_line(if self.artifact.pending_proposal().is_some() {
                    format!("artifact: replace proposed for line {line} (a=approve r=reject)")
                } else {
                    "artifact: empty document, nothing to replace".to_string()
                });
            }
            ArtifactMode::Insert => {
                if text.trim().is_empty() {
                    self.input.set(text);
                    return;
                }
                self.artifact.set_pending_insert(text);
                self.artifact.cancel_mode();
                self.system_line(if self.artifact.pending_proposal().is_some() {
                    format!("artifact: insert proposed below line {line} (a=approve r=reject)")
                } else {
                    "artifact: empty document, nothing to insert below".to_string()
                });
            }
            ArtifactMode::NaturalLanguage => {
                if text.trim().is_empty() {
                    self.input.set(text);
                    return;
                }
                // push_annotation mutates .artifact and ends; propose borrows
                // .proposer and .artifact immutably (disjoint fields); the
                // pending setter mutates .artifact.
                let Some(annotation) = self.artifact.push_annotation(text) else {
                    self.artifact.cancel_mode();
                    self.system_line("artifact: empty document, nothing to annotate");
                    return;
                };
                match self.proposer.propose(&self.artifact, &annotation) {
                    Ok(Some(proposal)) => {
                        self.artifact.set_pending(Some(proposal));
                        self.system_line(format!(
                            "artifact: NL proposal pending for line {line} (a=approve r=reject)"
                        ));
                    }
                    Ok(None) => self.system_line(
                        "natural-language proposer needs an LLM (not wired); use c/o/d for direct edits",
                    ),
                    Err(_) => self.system_line("artifact: proposer error (stub)"),
                }
                self.artifact.cancel_mode();
            }
            ArtifactMode::Normal => {}
        }
    }

    /// Propose deleting the focused line immediately (the d key). Builds the
    /// pending proposal on the session without entering an edit mode.
    pub(crate) fn artifact_propose_delete(&mut self) {
        let line = self.artifact.focus() + 1;
        self.artifact.set_pending_delete();
        self.system_line(if self.artifact.pending_proposal().is_some() {
            format!("artifact: delete proposed for line {line} (a=approve r=reject)")
        } else {
            "artifact: empty document, nothing to delete".to_string()
        });
    }

    /// Approve the design (one approval covering spec + plan). Both artifacts
    /// are marked approved and the chain advances to implementing.
    fn approve_design(&mut self) {
        if self.spec_artifact.approved && self.plan_artifact.approved {
            return;
        }
        self.spec_artifact.approved = true;
        self.plan_artifact.approved = true;
        self.enter_stage(
            Stage::Implementing,
            Pane::Diff,
            "design approved -> implementing",
        );
    }

    fn approve_focused_hunk(&mut self) {
        if !self.diff.approve_focused() {
            return;
        }
        let clause_id = self
            .diff
            .current()
            .map(|h| h.evidence.spec_clause_id.clone())
            .unwrap_or_default();
        // A change now implements the requirement: unimpl -> partial.
        self.set_clause_status(&clause_id, crate::state::Divergence::Partial);
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
            self.diff.focus_next_pending();
        }
    }

    fn reject_focused_hunk(&mut self) {
        if !self.diff.reject_focused() {
            return;
        }
        self.system_line("change rejected (requirement status unchanged)");
    }

    fn signoff_focused_finding(&mut self) {
        let before = self.review.audit_trail.len();
        self.signoff_focused("you", "now");
        if self.review.audit_trail.len() > before {
            self.system_line("finding approved -> decision log grew");
        }
        if self.all_findings_resolved() {
            // Agent review done -> machine-check phase (same verify stage).
            self.pane = Pane::Verify;
            self.system_line("agent review done -> machine check");
        }
    }

    fn reject_focused_finding(&mut self) {
        if let Some(note) = self.reject_focused("you", "now") {
            self.system_line(note);
        }
        if self.all_findings_resolved() {
            self.pane = Pane::Verify;
            self.system_line("agent review done -> machine check");
        }
    }

    fn complete_verify(&mut self) {
        // Verify passes only when every check is green. A failed-checks
        // state blocks completion and routes the user to the rework path
        // (r) instead.
        if !self.verify_result.passed {
            self.system_line("verify: checks failed -- press r to rework (stub)");
            return;
        }
        // Verify passes: every requirement with an approved change is satisfied.
        // Collect ids first to avoid borrowing self.diff while mutating clauses.
        let approved_clauses: Vec<String> = self
            .diff
            .hunks
            .iter()
            .filter(|h| h.approved == crate::state::Verdict::Approved)
            .map(|h| h.evidence.spec_clause_id.clone())
            .collect();
        for cid in approved_clauses {
            self.set_clause_status(&cid, crate::state::Divergence::Satisfied);
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
        match self.pane {
            Pane::Review if self.stage == Stage::Verify => self.rework_from_review(),
            Pane::Verify if self.stage == Stage::Verify => self.rework_from_verify(),
            _ => {}
        }
    }

    fn rework_from_review(&mut self) {
        let Some(f) = self.review.current() else {
            return;
        };
        if f.verdict != "real" {
            self.system_line("rework: only real findings trigger a rework (i)");
            return;
        }
        let clause_id = f.spec_clause_id.clone();
        let finding_id = f.id.clone();
        // Regress the requirement: a real finding means the change is incomplete.
        self.set_clause_status(&clause_id, crate::state::Divergence::Partial);
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
