//! Incremental transcript rebuilding from the ordered frame log.
//!
//! The visible history is bounded, and the stable prefix is reused while new
//! frames extend the current turn. Rewind and scrollback loading invalidate only
//! the affected range, keeping rebuild cost independent of session length.

use super::{MAX_REBUILD_FRAMES, PREPEND_BATCH};
use crate::records::TranscriptLine;
use crate::state::App;
use crate::transcript::{TranscriptFrame, transcript_from_frames};

impl App {
    /// Rebuild the transcript while preserving TUI-only lines at their current
    /// positions. The stable prefix is reused within a turn; a user boundary or
    /// rewind rebuilds the bounded visible history.
    pub(crate) fn rebuild_transcript(&mut self) {
        let turn_start = self.current_turn_start();
        // A changed turn boundary or truncated frame log invalidates the stable
        // prefix. Later frames in the same turn reuse it.
        let need_full = self.current_turn_boundary.frame_index > self.frames.len()
            || self.current_turn_boundary.frame_index != turn_start;
        if need_full {
            let frame_start = self.visible_frame_start();
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
                    merged.push(merge_subagent(line, event_lines[event_idx].clone()));
                    event_idx += 1;
                }
            }
            merged.extend(event_lines[event_idx..].iter().cloned());
            self.transcript = merged;
            self.current_turn_boundary.frame_index = turn_start;
            // Map the stable frame prefix to its transcript boundary while
            // retaining any interleaved TUI-only lines. Using transcript.len()
            // here would include the changing tail and duplicate it later.
            let prefix_line_count = if frame_start > 0 {
                transcript_from_frames(&self.frames[frame_start..turn_start.max(frame_start)]).len()
            } else {
                transcript_from_frames(&self.frames[..turn_start]).len()
            };
            let mut stable_end = 0;
            let mut non_tui = 0;
            for (i, line) in self.transcript.iter().enumerate() {
                if non_tui >= prefix_line_count {
                    stable_end = i;
                    break;
                }
                if !line.is_tui_only() {
                    non_tui += 1;
                }
                stable_end = i + 1;
            }
            self.current_turn_boundary.line_index = stable_end;
        } else {
            // Rebuild only the changing tail and preserve TUI-only lines at
            // their existing positions.
            let tail = transcript_from_frames(&self.frames[turn_start..]);
            let mut merged: Vec<TranscriptLine> =
                Vec::with_capacity(self.current_turn_boundary.line_index + tail.len());
            merged.extend(
                self.transcript[..self.current_turn_boundary.line_index]
                    .iter()
                    .cloned(),
            );
            let mut tail_idx = 0;
            for line in &self.transcript[self.current_turn_boundary.line_index..] {
                if line.is_tui_only() {
                    merged.push(line.clone());
                } else if tail_idx < tail.len() {
                    merged.push(merge_subagent(line, tail[tail_idx].clone()));
                    tail_idx += 1;
                }
            }
            merged.extend(tail[tail_idx..].iter().cloned());
            self.transcript = merged;
        }
        // Re-derive view caches incrementally from their frame cursors.
        self.accumulate_wire_state();
        self.bump_transcript_version();
    }

    /// Return the oldest frame included in the bounded transcript history.
    /// Scrollback may lower the boundary; normal rebuilds keep only the newest
    /// MAX_REBUILD_FRAMES frames. The loaded boundary is not advanced here,
    /// otherwise an initially empty session would permanently disable the cap.
    fn visible_frame_start(&self) -> usize {
        let window = self.frames.len().saturating_sub(MAX_REBUILD_FRAMES);
        window.min(self.loaded_from_frame.get())
    }

    /// Load an older frame batch when scrollback reaches the current history
    /// boundary, preserving the visible viewport position.
    pub(crate) fn load_older_frames(&mut self) {
        let from = self.visible_frame_start();
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
        // Load only when the user is near the current history boundary.
        if top > 5 {
            return;
        }
        let batch_start = from.saturating_sub(PREPEND_BATCH);
        let new_lines = transcript_from_frames(&self.frames[batch_start..from]);
        if new_lines.is_empty() {
            self.loaded_from_frame.set(batch_start);
            return;
        }
        let prepended = new_lines.len();
        // Insert the older lines before the currently loaded history.
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
        self.loaded_from_frame.set(batch_start);
        // Older lines extend the stable prefix and must survive the next tail
        // rebuild.
        self.current_turn_boundary.line_index += prepended;
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
        self.bump_transcript_version();
    }

    /// Return the first frame in the changing turn. If that boundary would
    /// split a tool call from its result, move it backward until the pair stays
    /// together.
    pub(crate) fn current_turn_start(&self) -> usize {
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
                // Move before this user boundary to keep the pair together.
                search_from = idx;
                continue;
            }
            return candidate;
        }
    }

    /// Whether the candidate boundary splits a tool call from a result that
    /// arrives later. A call with no result does not force the boundary back.
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

    /// Re-derive view caches from newly appended frames. Verdicts accumulate;
    /// todo-write applies last-write-wins only when a new checklist frame
    /// arrives, so an unrelated user boundary cannot clear active work.
    fn accumulate_wire_state(&mut self) {
        use houyicoder_protocol::acpx::AcpxMethod;
        use houyicoder_protocol::frontend::permission::PermissionDecisionEntry;
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
        self.todos.update(&self.frames, self.agent_busy);
    }
}

/// Preserve fetched child rows when rebuilding the same delegation. A new
/// delegation or a replacement that already has rows keeps its rebuilt value.
fn merge_subagent(old: &TranscriptLine, fresh: TranscriptLine) -> TranscriptLine {
    match (old, &fresh) {
        (
            TranscriptLine::Subagent {
                child_sid: old_sid,
                folded_transcript: old_rows,
                ..
            },
            TranscriptLine::Subagent {
                child_sid: new_sid,
                folded_transcript: new_rows,
                ..
            },
        ) if old_sid == new_sid && new_rows.is_empty() => {
            let mut out = fresh;
            if let TranscriptLine::Subagent {
                folded_transcript, ..
            } = &mut out
            {
                *folded_transcript = old_rows.clone();
            }
            out
        }
        _ => fresh,
    }
}
