//! Incremental transcript rebuilding from the ordered frame log.
//!
//! The visible history is bounded, and the stable prefix is reused while new
//! frames extend the current turn. Rewind and scrollback loading invalidate only
//! the affected range, keeping rebuild cost independent of session length.

use super::{MAX_REBUILD_FRAMES, PREPEND_BATCH};
use crate::records::TranscriptLine;
use crate::state::App;
use crate::transcript::{TranscriptFrame, is_run_completed, transcript_from_frames};

impl App {
    /// Rebuild the transcript from the frame log while preserving the expand
    /// state of the rows that reappear. The stable prefix is reused within a
    /// turn; a user boundary or rewind rebuilds the bounded visible history.
    /// A row the frontend raised renders from its own frame, so a rebuild
    /// re-derives it exactly where it was raised, and a window that no longer
    /// covers that frame drops it along with the rows around it.
    pub(crate) fn rebuild_transcript(&mut self) {
        let turn_start = self.current_turn_start();
        // A changed turn boundary or truncated frame log invalidates the stable
        // prefix. Later frames in the same turn reuse it. A record in that tail
        // forces the whole window too: the row a record derives needs the turn
        // it closes, and that turn opened before the tail begins, so folding
        // the tail alone could not name it.
        let record_in_tail = self.frames[turn_start..].iter().any(is_run_completed);
        let need_full = self.current_turn_boundary.frame_index > self.frames.len()
            || self.current_turn_boundary.frame_index != turn_start
            || record_in_tail;
        // A window of the frame log, not the whole of it: the derived rows
        // name their turn by log position, so the slice carries where it
        // starts, and a run in flight keeps its newest turn open.
        let newest_open = self.run_state.is_active();
        if need_full {
            let frame_start = self.visible_frame_start();
            let event_lines =
                transcript_from_frames(&self.frames, frame_start..self.frames.len(), newest_open);
            let mut merged: Vec<TranscriptLine> =
                Vec::with_capacity(self.transcript.len() + event_lines.len());
            let mut event_idx = 0;
            for line in &self.transcript {
                if event_idx < event_lines.len() && same_frame(line, &event_lines[event_idx]) {
                    merged.push(merge_subagent(line, event_lines[event_idx].clone()));
                    event_idx += 1;
                }
                // A row that does not pair here belongs to a frame outside the
                // window: it is dropped without consuming the fresh line, so
                // the next row pairs with its own rendering.
            }
            merged.extend_from_slice(&event_lines[event_idx..]);
            self.transcript = merged;
            self.current_turn_boundary.frame_index = turn_start;
            // Map the stable frame prefix to its transcript boundary: the
            // prefix's rows are the transcript's first rows, one per row the
            // projection derives from it, and that count says how much of this
            // transcript the next tail rebuild may reuse. A turn still open
            // where the prefix ends writes its row in the tail, so the tail
            // rebuild re-derives it; both counts read the same log facts and
            // agree line for line.
            let prefix_end = turn_start.max(frame_start);
            let prefix_line_count =
                transcript_from_frames(&self.frames, frame_start..prefix_end, newest_open).len();
            self.current_turn_boundary.line_index = prefix_line_count.min(self.transcript.len());
        } else {
            // Rebuild only the changing tail, pairing each row with the frame
            // it rendered so the expand state survives.
            let tail =
                transcript_from_frames(&self.frames, turn_start..self.frames.len(), newest_open);
            let mut merged: Vec<TranscriptLine> =
                Vec::with_capacity(self.current_turn_boundary.line_index + tail.len());
            merged.extend_from_slice(&self.transcript[..self.current_turn_boundary.line_index]);
            let mut tail_idx = 0;
            for line in &self.transcript[self.current_turn_boundary.line_index..] {
                if tail_idx < tail.len() && same_frame(line, &tail[tail_idx]) {
                    merged.push(merge_subagent(line, tail[tail_idx].clone()));
                    tail_idx += 1;
                } else if tail_idx < tail.len() && matches!(line, TranscriptLine::User(_)) {
                    // A local User echo whose own frame has not arrived in
                    // this tail: keep it at its position rather than treating
                    // the next event as its rendering. The echo reaches its
                    // own frame later and is paired then.
                    merged.push(line.clone());
                }
            }
            merged.extend_from_slice(&tail[tail_idx..]);
            self.transcript = merged;
        }
        // Bound the viewable transcript at the rebuild exit. A row the
        // frontend raises (a system line, a context view, an interrupt) grows
        // the transcript through this rebuild path, not the direct push path,
        // so the cap must sit here to cover both growth paths with one
        // ceiling. Trim shifts the turn boundary so a tail rebuild's stable
        // prefix stays in range; a scrolled-back reader is left alone so a
        // scroll-up session's loaded frames survive.
        self.trim_live_transcript();
        // Re-derive view caches incrementally from their frame cursors.
        self.accumulate_wire_state();
        self.bump_transcript_version();
    }

    /// Rebuild the whole visible window after a frame's payload changed in
    /// place. A reused prefix holds rows derived from the frames it covers, so
    /// a row whose frame changed under it would keep the former content until
    /// something else forced a full rebuild: the boundary is set one past the
    /// log, a position no frame holds, so the next rebuild takes that path.
    pub(crate) fn rebuild_after_frame_edit(&mut self) {
        self.current_turn_boundary.frame_index = self.frames.len() + 1;
        self.rebuild_transcript();
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
        if self.transcript_scroll.is_following_tail() {
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
        // The batch is a slice out of the middle of the log: the turn it
        // breaks off at the end continues into the lines already loaded, so
        // its summary row is not this batch's to write. A turn whose end lies
        // further up the log is another matter: this batch carries its frames,
        // so it folds that turn's row like any other window.
        let new_lines =
            transcript_from_frames(&self.frames, batch_start..from, self.run_state.is_active());
        if new_lines.is_empty() {
            self.loaded_from_frame.set(batch_start);
            return;
        }
        let prepended = new_lines.len();
        // Insert the older lines before the currently loaded history: the
        // batch's frames all precede the frames already loaded, and a row the
        // frontend raised among the batch's frames rides the batch's own
        // projection, so it lands where it was raised rather than above the
        // history it belongs inside.
        let mut merged = Vec::with_capacity(new_lines.len() + self.transcript.len());
        merged.extend(new_lines);
        merged.extend_from_slice(&self.transcript);
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
        self.todos.update(&self.frames, self.agent_busy());
    }
}

/// The conversational role a transcript line renders in, used to align the
/// visible history with a freshly rebuilt event line.
#[derive(PartialEq)]
enum LineRole {
    User,
    Agent,
    Tool,
    Thinking,
    Notice,
}

/// Whether both lines render the same frame: same role, and a row that belongs
/// to a frame names the same source — its tool call, its text, its child
/// session. Role alone mispairs once a slid window shifts the positions,
/// which leaves a notice inside a newer turn.
fn same_frame(visible: &TranscriptLine, fresh: &TranscriptLine) -> bool {
    use TranscriptLine::*;
    let role = |l: &TranscriptLine| match l {
        User(_) => LineRole::User,
        Agent(_) => LineRole::Agent,
        Tool { .. } => LineRole::Tool,
        Thinking { .. } => LineRole::Thinking,
        _ => LineRole::Notice,
    };
    match (visible, fresh) {
        (
            Tool {
                name: a_name,
                call_id: a_id,
                ..
            },
            Tool {
                name: b_name,
                call_id: b_id,
                ..
            },
        ) => (a_name == "result") == (b_name == "result") && a_id == b_id,
        (User(a), User(b)) | (Agent(a), Agent(b)) => a == b,
        (Thinking { text: a }, Thinking { text: b }) => a == b,
        // A turn summary row is named by where its turn ended, so the same
        // name is the same row: pairing by role alone would let a row whose
        // turn slid out of the window hand its expand state to a later turn.
        (ThoughtFor { turn_id: a, .. }, ThoughtFor { turn_id: b, .. }) => a == b,
        (Subagent { child_sid: a, .. }, Subagent { child_sid: b, .. }) => a == b,
        _ => role(visible) == role(fresh),
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
