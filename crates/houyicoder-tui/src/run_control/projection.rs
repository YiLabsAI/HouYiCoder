//! Transcript projection: turn the frame log App owns into the rendered
//! transcript. Split out of run_control so that file holds the run lifecycle
//! and the poll tick, not the projection.
//!
//! The projection is windowed. Only the newest MAX_PROJECT_FRAMES frames
//! project by default, and a dual cursor seals the frames of finished turns so
//! a rebuild during a turn re-projects only that turn. Both bounds matter: the
//! window keeps the cost independent of how long the session is, and the seal
//! keeps it independent of how long the current turn is.

use std::collections::HashSet;

use super::{MAX_PROJECT_FRAMES, PREPEND_BATCH};
use crate::records::TranscriptLine;
use crate::state::App;
use crate::transcript::{TranscriptFrame, transcript_from_frames};

impl App {
    /// Rebuild the transcript from App's own frame log, interleaving TUI-only
    /// lines (ContextGrid, System, Approval, slash-command echoes) at their
    /// held positions. Per-frame cost is O(current turn), not O(whole
    /// history): a dual cursor seals frames[..turn_start] (immutable for the
    /// run) into transcript[..sealed_transcript_len]; the mid-run path
    /// re-projects only frames[turn_start..] and merges only
    /// transcript[sealed_transcript_len..]. A full re-project runs once per
    /// turn boundary + after rewind truncates the seal invalid, and costs
    /// O(projection window), not O(history) -- the seal alone does not bound
    /// it, since a turn boundary invalidates the seal and the prefix is
    /// re-projected from the window start.
    pub(crate) fn rebuild_transcript(&mut self) {
        let turn_start = self.turn_seal_point();
        // Seal invalidation: rewind truncates frames below the seal, or the
        // turn boundary moved (a new UserMessageChunk arrived). Either way
        // re-project the prefix + re-seal. The prefix is immutable for the
        // rest of this turn, so subsequent mid-run rebuilds take the cheap
        // incremental path.
        let need_full =
            self.sealed_frames_end > self.frames.len() || self.sealed_frames_end != turn_start;
        if need_full {
            let frame_start = self.projection_start();
            let event_lines = if frame_start > 0 {
                transcript_from_frames(&self.frames[frame_start..])
            } else {
                transcript_from_frames(&self.frames)
            };
            let mut merged: Vec<TranscriptLine> =
                Vec::with_capacity(self.transcript.len() + event_lines.len());
            let mut event_idx = 0;
            for line in &self.transcript {
                if line.is_tui_only() {
                    merged.push(line.clone());
                } else if event_idx < event_lines.len() {
                    merged.push(event_lines[event_idx].clone());
                    event_idx += 1;
                }
            }
            merged.extend(event_lines[event_idx..].iter().cloned());
            self.transcript = merged;
            self.sealed_frames_end = turn_start;
            // sealed_transcript_len = the transcript index after the prefix's
            // frame-derived lines (frames[..turn_start] project to
            // prefix_line_count non-TUI-only lines; the TUI-only lines before
            // them stay interleaved in the prefix). This must NOT be
            // transcript.len(): a non-empty suffix (pair-safety fallback moves
            // turn_start back, or a batch of frames lands before a rebuild)
            // would otherwise seal the tail into the prefix + the next
            // incremental would append the tail again (silent duplicate).
            let prefix_line_count = if frame_start > 0 {
                transcript_from_frames(&self.frames[frame_start..turn_start.max(frame_start)]).len()
            } else {
                transcript_from_frames(&self.frames[..turn_start]).len()
            };
            let mut sealed = 0;
            let mut non_tui = 0;
            for (i, line) in self.transcript.iter().enumerate() {
                if non_tui >= prefix_line_count {
                    sealed = i;
                    break;
                }
                if !line.is_tui_only() {
                    non_tui += 1;
                }
                sealed = i + 1;
            }
            self.sealed_transcript_len = sealed;
        } else {
            // Incremental: re-project only the current turn's frames. The
            // prefix (frames[..turn_start] projected into
            // transcript[..sealed_transcript_len]) is immutable; merge only
            // the tail region, preserving held TUI-only lines at positions.
            let tail = transcript_from_frames(&self.frames[turn_start..]);
            let mut merged: Vec<TranscriptLine> =
                Vec::with_capacity(self.sealed_transcript_len + tail.len());
            merged.extend(
                self.transcript[..self.sealed_transcript_len]
                    .iter()
                    .cloned(),
            );
            let mut tail_idx = 0;
            for line in &self.transcript[self.sealed_transcript_len..] {
                if line.is_tui_only() {
                    merged.push(line.clone());
                } else if tail_idx < tail.len() {
                    merged.push(tail[tail_idx].clone());
                    tail_idx += 1;
                }
            }
            merged.extend(tail[tail_idx..].iter().cloned());
            self.transcript = merged;
        }
        // Re-derive the view caches incrementally: verdicts parse only past
        // the verdict cursor, todos scan only the current turn (turn_start).
        self.accumulate_wire_state(turn_start);
        let v = self.transcript_version.get().wrapping_add(1);
        self.transcript_version.set(v);
    }

    /// The oldest frame the transcript projects: the newest MAX_PROJECT_FRAMES,
    /// pulled further back when a scroll-up prepend asked for older frames.
    ///
    /// projected_from_frame holds only what a prepend asked for and stays at
    /// MAX until one happens, so the window bound wins by default. Writing the
    /// computed start back into it instead would latch the window open: a
    /// session starts with an empty frame log, whose window start is 0, so the
    /// first rebuild would pin the floor to 0 and the min could never rise
    /// again. Every later rebuild would then re-project the whole history,
    /// which costs the frame count squared over a replay -- a resumed session
    /// of 17k frames took over a minute to paint anything, while a 700-frame
    /// one stayed fast enough to look correct.
    fn projection_start(&self) -> usize {
        let window = self.frames.len().saturating_sub(MAX_PROJECT_FRAMES);
        window.min(self.projected_from_frame.get())
    }

    /// Progressive prepend: when the user scrolls to the top of the projected
    /// region and there are unprojected frames above, project a batch of
    /// older frames and prepend them to the transcript. Adjusts the scroll
    /// offset so the user's viewport stays stable (the newly prepended rows
    /// push the old top down by prepended_count). An on-demand
    /// mount on scroll-up: data is in memory, only the projection
    /// (render) is lazy.
    pub(crate) fn ensure_projected_above(&mut self) {
        let from = self.projection_start();
        if from == 0 {
            return;
        }
        // Don't prepend when following the tail (user is at the bottom).
        if self.transcript_scroll.follow_tail {
            return;
        }
        let top = self
            .transcript_scroll
            .top_offset(self.display_rows_cache.borrow().len());
        // Only prepend when the user is near the top of the projected region.
        if top > 5 {
            return;
        }
        let batch_start = from.saturating_sub(PREPEND_BATCH);
        let new_lines = transcript_from_frames(&self.frames[batch_start..from]);
        if new_lines.is_empty() {
            self.projected_from_frame.set(batch_start);
            return;
        }
        let prepended = new_lines.len();
        // Prepend: the new lines go before the existing projected region.
        // TUI-only lines at the top of the transcript (system messages pushed
        // before any frame) stay above the prepended frame-derived lines.
        let mut split = 0;
        for (i, line) in self.transcript.iter().enumerate() {
            if !line.is_tui_only() {
                split = i;
                break;
            }
            split = i + 1;
        }
        let mut merged = Vec::with_capacity(new_lines.len() + self.transcript.len());
        merged.extend(self.transcript[..split].iter().cloned());
        merged.extend(new_lines);
        merged.extend(self.transcript[split..].iter().cloned());
        self.transcript = merged;
        self.projected_from_frame.set(batch_start);
        // The prepended lines are all part of the sealed prefix (they
        // correspond to frames below sealed_frames_end). Update the
        // transcript-side seal cursor so the next incremental rebuild
        // treats them as immutable prefix, not tail to re-project.
        self.sealed_transcript_len += prepended;
        // Shift the scroll position down by the prepended count so the
        // viewport content stays stable. NOTE: prepended counts
        // TranscriptLines, not display rows — multi-row lines (Agent,
        // Tool results) cause under-adjustment. The next draw_transcript
        // recomputes from the cache which corrects the viewport. The
        // one-frame drift is acceptable (prepend only fires on scroll-up,
        // the user is actively scrolling, not reading a static view).
        let cur = self.transcript_scroll.raw_top();
        self.transcript_scroll.set_raw_top(cur + prepended);
        // Invalidate the display cache (transcript changed).
        let v = self.transcript_version.get().wrapping_add(1);
        self.transcript_version.set(v);
    }

    /// The current turn's seal point: the index after the last
    /// UserMessageChunk (the run's user-prompt boundary; TurnAborted is
    /// projected as UserMessageChunk too, so an interrupt also starts a fresh
    /// seal — safe, only over-seals). Pair-safety: if the prefix would leave
    /// a ToolCall whose ToolCallUpdate is in the tail (the call/result pair
    /// would split, orphaning the result when the tail is re-projected
    /// alone — the abort path's reconcile_tool_results can shape this),
    /// fall back to the UserMessageChunk before it so the pair stays together
    /// in the tail. Bounded loop: each fallback moves to an earlier
    /// UserMessageChunk; at worst reaches the first one.
    pub(crate) fn turn_seal_point(&self) -> usize {
        use houyicoder_protocol::frontend::session_update::SessionUpdate;
        let mut search_from = self.frames.len();
        loop {
            let Some(idx) = self.frames[..search_from].iter().rposition(|f| {
                matches!(
                    f,
                    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_))
                )
            }) else {
                return 0;
            };
            let candidate = idx + 1;
            if self.prefix_has_unpaired_call(candidate) {
                // The pair would split across the seal; retry from before
                // this UserMessageChunk so the call+result stay in the tail.
                search_from = idx;
                continue;
            }
            return candidate;
        }
    }

    /// Whether frames[..candidate] contains a ToolCall whose ToolCallUpdate
    /// Whether frames[..candidate] contains a ToolCall whose ToolCallUpdate
    /// lands in the TAIL (frames[candidate..]) — the pair would split across
    /// the seal, orphaning the result when the tail is re-projected alone. A
    /// call with NO result anywhere (a hanging ToolCall — the cross-process
    /// resume norm, where the prior run was interrupted before the tool
    /// completed) has nothing to orphan, so it does NOT trigger a fallback.
    /// Without this, a single hanging call would make every candidate unpaired
    /// → the fallback bottoms out at 0 → the whole session re-projects fully
    /// per frame (the optimization gone on the longest sessions).
    fn prefix_has_unpaired_call(&self, candidate: usize) -> bool {
        use houyicoder_protocol::frontend::session_update::SessionUpdate;
        let mut calls = std::collections::HashSet::new();
        let mut results = std::collections::HashSet::new();
        for f in &self.frames[..candidate] {
            match f {
                TranscriptFrame::Session(SessionUpdate::ToolCall(tc)) => {
                    calls.insert(tc.tool_call_id.0.as_str());
                }
                TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(upd)) => {
                    results.insert(upd.tool_call_id.0.as_str());
                }
                _ => {}
            }
        }
        // A tail result for a prefix call (split). Hanging calls (no result
        // anywhere) are not in tail_results, so they do not trigger.
        let mut tail_results = std::collections::HashSet::new();
        for f in &self.frames[candidate..] {
            if let TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(upd)) = f {
                tail_results.insert(upd.tool_call_id.0.as_str());
            }
        }
        calls
            .iter()
            .any(|id| !results.contains(id) && tail_results.contains(id))
    }

    /// Re-derive view caches from the frame log. The verdict log deserializes
    /// each acpx/context/permission_decision notification into a typed entry;
    /// the checklist parses each todo-write tool call's input into a typed
    /// view list (last-write-wins: the tool posts the full list each call, and
    /// an all-done call clears it). Incremental: verdicts parse only frames
    /// past the verdict cursor (append-only), todos scan only the current turn
    /// (turn_start passed in, no full rposition). O(new) per call, not
    /// O(history).
    fn accumulate_wire_state(&mut self, turn_start: usize) {
        use houyicoder_protocol::acpx::AcpxMethod;
        use houyicoder_protocol::frontend::permission::PermissionDecisionEntry;
        use houyicoder_protocol::frontend::session_update::SessionUpdate;
        // Verdicts are append-only (audit trail). Rewind/clear truncates frames
        // below the cursor → reset + re-parse from 0 so the cache matches the
        // truncated log (no stale verdicts for dropped frames).
        if self.verdict_cursor > self.frames.len() {
            self.verdict_cursor = 0;
            self.verdict_log_cache.clear();
        }
        for f in self.frames.iter().skip(self.verdict_cursor) {
            if let TranscriptFrame::Acpx(n) = f
                && matches!(n.method, AcpxMethod::ContextPermissionDecision)
                && let Ok(entry) =
                    serde_json::from_value::<PermissionDecisionEntry>(n.params.clone())
            {
                self.verdict_log_cache.push(entry);
            }
        }
        self.verdict_cursor = self.frames.len();
        let mut todos: Option<Vec<crate::todo_view::TodoView>> = None;
        // Project todos only from the current run (turn_start = the seal point,
        // already computed in rebuild_transcript). A run with no todo-write
        // leaves todos empty so a prior run's all-completed list does not leak
        // as an orphan block into the next turn.
        for f in self.frames.iter().skip(turn_start) {
            if let TranscriptFrame::Session(update) = f
                && matches!(update, SessionUpdate::ToolCall(_))
                && let Some(parsed) = crate::todo_view::from_tool_call(update)
            {
                todos = Some(parsed);
            }
        }
        let new_todos = todos.unwrap_or_default();
        // Track completion timestamps: stamp any item that is completed in the
        // new list but was not completed (or absent) in the old list. This
        // drives the 30-second recent-completed visibility window. Skipped on
        // the initial projection (old cache empty): items completed before
        // this process attached are historic, not recent — stamping them
        // would flood the collapsed list for 30 seconds after a resume.
        let initial_projection = self.todos_cache.is_empty();
        if !initial_projection {
            let old_completed: HashSet<String> = self
                .todos_cache
                .iter()
                .filter(|t| t.status == crate::todo_view::TodoStatus::Completed)
                .map(|t| t.content.clone())
                .collect();
            let now = std::time::Instant::now();
            for t in &new_todos {
                if t.status == crate::todo_view::TodoStatus::Completed
                    && !old_completed.contains(&t.content)
                {
                    self.todo_completion_at.insert(t.content.clone(), now);
                }
            }
        }
        // Remove timestamps for items no longer completed (e.g. a new
        // todo-write frame reused the content with a different status).
        let new_completed: HashSet<String> = new_todos
            .iter()
            .filter(|t| t.status == crate::todo_view::TodoStatus::Completed)
            .map(|t| t.content.clone())
            .collect();
        self.todo_completion_at
            .retain(|k, _| new_completed.contains(k));
        self.todos_cache = new_todos;
    }
}
