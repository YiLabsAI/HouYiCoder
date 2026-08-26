//! App methods extracted from state.rs so state.rs stays under the
//! size gate with room for field doc restoration.

use crate::state::{App, Divergence, Stage, TranscriptLine, Verdict, ViewportMode};
use houyicoder_protocol::frontend::SlashCommand;

impl App {
    /// The filtered slash-command list, narrowed by the inline query.
    pub fn palette_filtered(&self) -> Vec<SlashCommand> {
        self.palette.filtered()
    }

    /// Number of palette entries after filtering.
    pub fn palette_len(&self) -> usize {
        self.palette.len()
    }

    /// The currently selected slash command, if the palette is open and the
    /// filtered list is non-empty.
    pub fn selected_command(&self) -> Option<SlashCommand> {
        self.palette.selected()
    }

    /// Move the palette selection up, wrapping around the filtered list.
    pub fn palette_up(&mut self) {
        self.palette.prev();
    }

    /// Move the palette selection down, wrapping around the filtered list.
    pub fn palette_down(&mut self) {
        self.palette.next();
    }

    /// Push a character onto the inline filter query and reset the selection.
    pub fn palette_push(&mut self, c: char) {
        self.palette.push(c);
    }

    /// Remove the trailing character of the inline filter query.
    pub fn palette_pop(&mut self) {
        self.palette.pop();
    }

    /// Open the palette with an empty filter at the first command.
    pub fn open_palette(&mut self) {
        self.palette.open();
    }

    /// Close the palette without running a command.
    pub fn close_palette(&mut self) {
        self.palette.close();
    }

    /// Toggle plan mode on or off.
    pub fn toggle_plan(&mut self) {
        self.status.plan_mode = !self.status.plan_mode;
    }

    /// Push a system line onto the transcript. The scroll state is left
    /// untouched (see push_transcript_line); a user following the tail stays
    /// on the tail via top_offset, a user scrolled back stays scrolled back.
    pub fn system_line(&mut self, msg: impl Into<String>) {
        self.push_transcript_line(TranscriptLine::System(msg.into()));
    }

    /// Push a transcript line without touching the scroll state. A user
    /// following the tail stays on the tail via top_offset on the next draw;
    /// a user scrolled back stays in place — agent output never yanks a
    /// reader of history. User-initiated re-follow (input, End, Ctrl+End,
    /// PageDown to bottom) happens at the action site, not here.
    pub fn push_transcript_line(&mut self, line: TranscriptLine) {
        self.transcript.push(line);
        crate::scroll::bound_scrollback(&mut self.transcript);
        let v = self.transcript_version.get().wrapping_add(1);
        self.transcript_version.set(v);
    }

    // The page/line scroll methods (scroll_transcript_up / down /
    // follow_tail / line_up / line_down + the debug_scroll helper) live in
    // state_scroll.rs so this file stays under the file-size gate. The impl
    // block there adds them to App; callers reach them as self.scroll_*.

    /// Transition to a new stage, pushing the previous one onto the history
    /// stack so /rewind can return to it. Also updates the spec strip step and
    /// syncs the viewport to the new stage (implement/verify fold into Focus).
    pub fn set_stage(&mut self, stage: Stage) {
        self.stage_history.push(self.stage);
        self.stage = stage;
        self.spec_ctx.step = stage.label().to_string();
        self.sync_viewport_to_stage();
    }

    /// Auto-set the viewport from the current stage. Implement and verify fold
    /// into Focus; idle, design, and done unfold to Working. Also closes any
    /// open palette/search when leaving Working so the inline-only layout
    /// cells do not linger.
    pub fn sync_viewport_to_stage(&mut self) {
        let next = ViewportMode::for_stage(self.stage);
        if next != ViewportMode::Working {
            self.palette.close();
            self.search.close();
        }
        self.viewport = next;
    }

    /// Leave Scroll mode and restore the prior viewport. Only switches
    /// viewport — does NOT return to the tail, so typing or opening a search
    /// from scroll mode keeps the view (spec: typing keeps the pill). Esc
    /// and End add follow_tail at their call sites for "Esc=tail".
    pub fn exit_scroll(&mut self) {
        self.viewport = self.prev_viewport;
    }

    /// Fold Focus into Working (user pressed Esc in Focus). Stage unchanged.
    pub fn fold_to_working(&mut self) {
        self.viewport = ViewportMode::Working;
    }

    /// Pop the most recent stage off the history stack and restore it. Returns
    /// the restored stage when there was something to rewind to. Also syncs the
    /// viewport so rewinding from Focus back to design unfolds to Working.
    pub fn rewind_stage(&mut self) -> Option<Stage> {
        if self.stage_history.is_empty() {
            return None;
        }
        let prev = self.stage_history.pop().expect("non-empty");
        self.stage = prev;
        self.spec_ctx.step = prev.label().to_string();
        self.sync_viewport_to_stage();
        Some(prev)
    }

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
    /// rejected). Used to auto-advance from Implementing to Verify (review pane).
    pub fn all_hunks_approved(&self) -> bool {
        !self.diff.hunks.is_empty()
            && self
                .diff
                .hunks
                .iter()
                .all(|h| h.approved == Verdict::Approved)
    }

    /// True when every review finding has been signed off or rejected.
    pub fn all_findings_resolved(&self) -> bool {
        !self.review.findings.is_empty() && self.review.findings.iter().all(|f| f.resolved())
    }

    /// Number of review findings in the console queue.
    pub fn console_len(&self) -> usize {
        self.review.len()
    }

    /// Move the console review-queue focus up, wrapping around.
    pub fn console_focus_up(&mut self) {
        self.review.focus_up();
    }

    /// Move the console review-queue focus down, wrapping around.
    pub fn console_focus_down(&mut self) {
        self.review.focus_down();
    }

    /// Sign off on the focused finding: mark signed off and append an audit
    /// trail entry. No-op if the queue is empty or already signed off.
    pub fn signoff_focused(&mut self, who: &str, when: &str) {
        self.review.signoff_focused(who, when);
    }

    /// Reject the focused finding: mark rejected, write back to org eval
    /// (stub), and append an audit trail entry. Returns the org-eval feedback
    /// note when a rejection happened.
    pub fn reject_focused(&mut self, who: &str, when: &str) -> Option<String> {
        self.review.reject_focused(who, when)
    }
}
