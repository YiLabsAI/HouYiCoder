//! Checklist view model accumulated from the wire stream. The agent's
//! todo-write tool calls ride the transcript as ordinary tool calls whose
//! input carries the new task list; this module parses that wire payload
//! into a typed view model the render layer reads. The data is already on
//! the wire (the transcript renders tool calls from the same input), so
//! this is client-side view derivation, not engine-state coupling.
//!
//! Last-write-wins: the tool posts the full list each call, so the most
//! recent todo-write frame determines the current checklist. The accumulator
//! advances an append-only frame cursor; unrelated user boundaries retain the
//! last checklist instead of clearing it. A cold projection, idle replay or
//! rewind, is restored history rather than a live event: it records no
//! completion timestamps, and an all-completed list clears on the spot
//! instead of installing.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use houyicoder_protocol::frontend::session_update::{SessionUpdate, ToolCall};

use crate::state::EventCursor;
use crate::transcript::TranscriptFrame;

/// Time a completed task remains visible before clearing from the transcript.
pub(crate) const RECENT_COMPLETION_TTL: Duration = Duration::from_secs(30);
const COMPLETED_LIST_TTL: Duration = Duration::from_secs(5);

/// Checklist status, including the paused state used for resumed idle work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Paused,
    Completed,
}

impl TodoStatus {
    /// Parse the snake-case status string the tool input carries. Unknown
    /// values degrade to Pending so a forward-incompatible payload never hides
    /// a task from the view.
    pub fn from_snake(s: &str) -> Self {
        match s {
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            _ => Self::Pending,
        }
    }
}

/// One checklist entry: the content line, its status, and the optional
/// active-form label shown for the in-progress task (a short verb phrase
/// describing the work underway, e.g. running tests for a run-tests task).
#[derive(Debug, Clone)]
pub struct TodoView {
    pub content: String,
    pub status: TodoStatus,
    pub active_form: Option<String>,
}

/// Projected checklist state and its completion visibility lifecycle.
#[derive(Default)]
pub struct TodoState {
    pub(crate) items: Vec<TodoView>,
    pub(crate) expanded: bool,
    pub(crate) completion_at: HashMap<String, Instant>,
    cursor: Option<EventCursor>,
    /// True while restored history (a resume replay or a rewind) is being
    /// re-fed into the accumulator. Every appended frame during that phase is
    /// a snapshot restore, not a live transition, so no completion timestamp
    /// records and an all-completed list clears on the spot.
    replaying_history: bool,
}

impl TodoState {
    /// Apply newly appended todo-write frames using last-write-wins semantics.
    /// The cursor is an absolute frame boundary and abs_base is the absolute
    /// index of frames[0], so a front drain shifts neither: the scan resumes
    /// where it left off instead of re-deriving from a moved front and clearing
    /// a checklist whose only todo-write frame was drained.
    pub(crate) fn update<F: AsRef<TranscriptFrame>>(
        &mut self,
        frames: &[F],
        abs_base: usize,
        run_active: bool,
    ) {
        let abs_count = abs_base + frames.len();
        let (start, reset) = match self.cursor {
            None => (0, false),
            Some(EventCursor::Local(n)) => {
                let n = n as usize;
                if n > abs_count {
                    (0, true)
                } else {
                    (n.saturating_sub(abs_base), false)
                }
            }
            // The generic slice carries no event seq; re-derive from the front.
            Some(EventCursor::Server(_)) => (0, true),
        };
        if reset {
            self.items.clear();
            self.completion_at.clear();
        }
        let mut latest = None;
        for frame in frames.iter().skip(start) {
            if let TranscriptFrame::Session(update) = frame.as_ref()
                && let Some(parsed) = from_tool_call(update)
            {
                latest = Some(parsed);
            }
        }
        self.cursor = Some(EventCursor::Local(abs_count as u64));
        if !run_active {
            pause_inactive(&mut self.items);
        }
        let Some(mut items) = latest else {
            return;
        };
        if !run_active {
            pause_inactive(&mut items);
        }
        // Cold is identified, not inferred: the App marks the replay phase
        // while restored history or a rewind is re-fed into the accumulator.
        // Such frames restore a snapshot rather than transitioning live, so
        // they record no completion timestamps and an all-completed list
        // clears on the spot. The phase persists across every replayed frame,
        // so a list that finishes on a later frame still reads as cold.
        let cold_projection = self.replaying_history;
        if cold_projection
            && !items.is_empty()
            && items
                .iter()
                .all(|item| item.status == TodoStatus::Completed)
        {
            // A restored all-completed list is finished history; clear it
            // on the spot instead of installing items for the next prune.
            // A late replayed frame must also drop the partial list an
            // earlier frame installed, so clear rather than merely return.
            self.items.clear();
            self.completion_at.clear();
            self.expanded = false;
            return;
        }
        if !cold_projection {
            let old_completed: HashSet<String> = self
                .items
                .iter()
                .filter(|item| item.status == TodoStatus::Completed)
                .map(|item| item.content.clone())
                .collect();
            let now = Instant::now();
            for item in &items {
                if item.status == TodoStatus::Completed && !old_completed.contains(&item.content) {
                    self.completion_at.insert(item.content.clone(), now);
                }
            }
        }
        let completed: HashSet<String> = items
            .iter()
            .filter(|item| item.status == TodoStatus::Completed)
            .map(|item| item.content.clone())
            .collect();
        // Drop timestamps for content the new list no longer shows as
        // completed.
        self.completion_at
            .retain(|content, _| completed.contains(content));
        if !cold_projection
            && !items.is_empty()
            && items
                .iter()
                .all(|item| item.status == TodoStatus::Completed)
        {
            let now = Instant::now();
            for item in &items {
                self.completion_at
                    .entry(item.content.clone())
                    .or_insert(now);
            }
        }
        self.items = items;
    }

    /// Hide the whole checklist after every item has remained completed for
    /// the completion visibility window. An untimestamped completed list also
    /// clears as a defensive fallback; cold projections normally clear in
    /// update before installing any items.
    pub(crate) fn prune(&mut self, now: Instant) -> bool {
        if self.items.is_empty()
            || self
                .items
                .iter()
                .any(|item| item.status != TodoStatus::Completed)
            || self
                .completion_at
                .values()
                .max()
                .is_some_and(|completed| now.duration_since(*completed) < COMPLETED_LIST_TTL)
        {
            return false;
        }
        self.items.clear();
        self.completion_at.clear();
        self.expanded = false;
        true
    }

    /// Reset checklist content, expansion, timestamps, and frame cursor.
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Mark the replay phase as begun or ended. The App sets this while
    /// restored history or a rewind is re-fed into the accumulator and clears
    /// it when a live run starts.
    pub(crate) fn set_replaying_history(&mut self, replaying_history: bool) {
        self.replaying_history = replaying_history;
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        match self.cursor {
            Some(EventCursor::Local(n)) => n as usize,
            _ => 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_cursor(&mut self, cursor: usize) {
        self.cursor = Some(EventCursor::Local(cursor as u64));
    }
}

fn pause_inactive(items: &mut [TodoView]) {
    for item in items {
        if item.status == TodoStatus::InProgress {
            item.status = TodoStatus::Paused;
        }
    }
}

/// Parse a todo-write tool call's input into the view list. The input shape
/// matches what the tool itself parses on the engine side: a todos array
/// whose items carry content, status, and an optional activeForm. Returns
/// None when the frame is not a valid todo-write update; Some(vec) when it is.
/// An explicit empty todos array clears the checklist, while malformed input
/// leaves the last valid state intact.
pub fn from_tool_call(update: &SessionUpdate) -> Option<Vec<TodoView>> {
    let SessionUpdate::ToolCall(ToolCall {
        title, raw_input, ..
    }) = update
    else {
        return None;
    };
    if title != "todo_write" {
        return None;
    }
    let arr = raw_input
        .as_ref()
        .and_then(|v| v.get("todos"))
        .and_then(|v| v.as_array());
    let Some(arr) = arr else {
        tracing::warn!("ignored todo-write frame without a todos array");
        return None;
    };
    let views = arr
        .iter()
        .filter_map(|item| {
            let content = item.get("content")?.as_str()?.to_string();
            if content.is_empty() {
                return None;
            }
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .map(TodoStatus::from_snake)
                .unwrap_or(TodoStatus::Pending);
            let active_form = item
                .get("activeForm")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            Some(TodoView {
                content,
                status,
                active_form,
            })
        })
        .collect();
    Some(views)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::slice::from_ref;

    use houyicoder_protocol::envelope::EventSeq;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::{
        ContentChunk, SessionUpdate, ToolCall, ToolCallId,
    };

    fn todo_write_frame(todos_json: serde_json::Value) -> SessionUpdate {
        let mut tc = ToolCall::new(ToolCallId::new("c1"), "todo_write");
        tc.raw_input = Some(todos_json);
        SessionUpdate::ToolCall(tc)
    }

    fn non_todo_frame() -> SessionUpdate {
        SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text { text: "hi".into() }))
    }

    #[test]
    fn test_from_tool_parses_items() {
        let payload = serde_json::json!({
            "todos": [
                { "content": "run tests", "status": "in_progress", "activeForm": "running tests" },
                { "content": "write docs", "status": "pending" },
                { "content": "ship", "status": "completed" }
            ]
        });
        let views = from_tool_call(&todo_write_frame(payload)).expect("todo-write frame");
        assert_eq!(views.len(), 3);
        assert_eq!(views[0].status, TodoStatus::InProgress);
        assert_eq!(views[0].active_form.as_deref(), Some("running tests"));
        assert_eq!(views[1].status, TodoStatus::Pending);
        assert!(views[1].active_form.is_none());
        assert_eq!(views[2].status, TodoStatus::Completed);
    }

    #[test]
    fn test_ignores_non_todo_calls() {
        assert!(from_tool_call(&non_todo_frame()).is_none());
    }

    #[test]
    fn test_missing_payload_ignored() {
        let mut tc = ToolCall::new(ToolCallId::new("c1"), "todo_write");
        tc.raw_input = None;
        assert!(from_tool_call(&SessionUpdate::ToolCall(tc)).is_none());
    }

    #[test]
    fn test_empty_payload_clears() {
        let views = from_tool_call(&todo_write_frame(serde_json::json!({ "todos": [] })))
            .expect("valid empty update");
        assert!(views.is_empty());
    }

    #[test]
    fn test_from_snake_maps_unknown() {
        assert_eq!(TodoStatus::from_snake("pending"), TodoStatus::Pending);
        assert_eq!(
            TodoStatus::from_snake("in_progress"),
            TodoStatus::InProgress
        );
        assert_eq!(TodoStatus::from_snake("completed"), TodoStatus::Completed);
        assert_eq!(TodoStatus::from_snake("bogus"), TodoStatus::Pending);
    }

    #[test]
    fn test_run_end_pauses_existing() {
        let frame = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
            "todos": [{"content": "unfinished", "status": "in_progress"}]
        })));
        let mut state = TodoState::default();
        state.update(from_ref(&frame), 0, true);
        assert_eq!(state.items[0].status, TodoStatus::InProgress);

        state.update(from_ref(&frame), 0, false);
        assert_eq!(state.items[0].status, TodoStatus::Paused);
    }

    #[test]
    fn test_prune_hides_done() {
        let now = Instant::now();
        let mut state = TodoState {
            items: vec![TodoView {
                content: "done".into(),
                status: TodoStatus::Completed,
                active_form: None,
            }],
            completion_at: HashMap::from([("done".to_string(), now - Duration::from_secs(6))]),
            ..Default::default()
        };

        assert!(state.prune(now));
        assert!(state.items.is_empty());
        assert!(state.completion_at.is_empty());
    }

    #[test]
    fn test_prune_keeps_open() {
        let now = Instant::now();
        let mut state = TodoState {
            items: vec![TodoView {
                content: "open".into(),
                status: TodoStatus::Pending,
                active_form: None,
            }],
            completion_at: HashMap::new(),
            ..Default::default()
        };

        assert!(!state.prune(now));
        assert_eq!(state.items.len(), 1);
    }

    /// A cold replay of an all-completed list clears on the spot: the view
    /// installs nothing and records no timestamps, and every fresh
    /// accumulator (each resume) behaves the same.
    #[test]
    fn test_cold_replay_clears() {
        let frame = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
            "todos": [{"content": "done", "status": "completed"}]
        })));
        for _ in 0..3 {
            let mut state = TodoState::default();
            state.set_replaying_history(true);
            state.update(from_ref(&frame), 0, false);
            assert!(state.items.is_empty());
            assert!(state.completion_at.is_empty());
            assert!(!state.prune(Instant::now()), "nothing left to prune");
        }
    }

    /// A resume replays the transcript across several frames, not one. The
    /// all-completed list that lands on a later replayed frame still reads as
    /// cold: the partial list an earlier frame installed clears instead of
    /// flashing the finished list with a fresh timestamp.
    #[test]
    fn test_replay_later_frame_cold() {
        let open = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
            "todos": [
                {"content": "one", "status": "in_progress"},
                {"content": "two", "status": "pending"}
            ]
        })));
        let partial = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
            "todos": [
                {"content": "one", "status": "completed"},
                {"content": "two", "status": "pending"}
            ]
        })));
        let finished = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
            "todos": [
                {"content": "one", "status": "completed"},
                {"content": "two", "status": "completed"}
            ]
        })));
        let mut frames: Vec<TranscriptFrame> = Vec::new();
        let mut state = TodoState::default();
        state.set_replaying_history(true);

        frames.push(open);
        state.update(&frames, 0, false);
        assert_eq!(state.items.len(), 2);

        frames.push(partial);
        state.update(&frames, 0, false);
        assert_eq!(state.items.len(), 2);

        frames.push(finished);
        state.update(&frames, 0, false);
        assert!(state.items.is_empty());
        assert!(state.completion_at.is_empty());
    }

    /// A rewind truncates the transcript below the cursor. The rollback
    /// drops items and completion timestamps together, so the replayed
    /// history cannot re-enter the completion visibility window.
    #[test]
    fn test_rewind_clears_completion() {
        let frame = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
            "todos": [{"content": "done", "status": "completed"}]
        })));
        let mut state = TodoState::default();
        state.update(from_ref(&frame), 0, true);
        assert!(state.completion_at.contains_key("done"));

        state.set_cursor(4);
        state.set_replaying_history(true);
        state.update(from_ref(&frame), 0, false);
        assert!(state.items.is_empty());
        assert!(state.completion_at.is_empty());
    }

    /// A server-anchored cursor cannot be resolved against the generic frame
    /// slice, so the accumulator re-derives from the front; the usize test
    /// accessor reads a server cursor back as zero.
    #[test]
    fn test_server_cursor_re_derives() {
        let frame = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
            "todos": [{"content": "done", "status": "completed"}]
        })));
        let mut state = TodoState {
            cursor: Some(EventCursor::Server(EventSeq(7))),
            ..Default::default()
        };
        assert_eq!(
            state.cursor(),
            0,
            "the usize accessor reads a server cursor as zero"
        );
        state.update(from_ref(&frame), 0, true);
        assert_eq!(
            state.items.len(),
            1,
            "server cursor re-derives from the front"
        );
    }

    /// A live run reaching the all-completed state records timestamps and
    /// keeps the list visible for the grace window.
    #[test]
    fn test_live_done_timestamped() {
        let frame = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
            "todos": [{"content": "done", "status": "completed"}]
        })));
        let mut state = TodoState::default();
        state.update(from_ref(&frame), 0, true);

        assert!(state.completion_at.contains_key("done"));
        assert!(!state.prune(Instant::now()));
    }

    /// An idle replay with open work stays visible; only the completion
    /// timestamp recording is suppressed.
    #[test]
    fn test_replay_open_kept() {
        let frame = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
            "todos": [
                {"content": "done", "status": "completed"},
                {"content": "open", "status": "pending"}
            ]
        })));
        let mut state = TodoState::default();
        state.set_replaying_history(true);
        state.update(from_ref(&frame), 0, false);

        assert_eq!(state.items.len(), 2);
        assert!(state.completion_at.is_empty());
        assert!(!state.prune(Instant::now()));
    }

    /// A live run completing the last open item after a restored mixed list
    /// records timestamps for the whole list (one shared grace window);
    /// the restore itself recorded none.
    #[test]
    fn test_live_completion_after_restore() {
        let mixed = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
            "todos": [
                {"content": "old", "status": "completed"},
                {"content": "current", "status": "in_progress"}
            ]
        })));
        let mut state = TodoState::default();
        state.set_replaying_history(true);
        state.update(from_ref(&mixed), 0, false);
        assert!(state.completion_at.is_empty());
        state.set_replaying_history(false);

        let finished = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
            "todos": [
                {"content": "old", "status": "completed"},
                {"content": "current", "status": "completed"}
            ]
        })));
        let frames = vec![mixed, finished];
        state.update(&frames, 0, true);

        assert!(state.completion_at.contains_key("current"));
        assert!(state.completion_at.contains_key("old"));
    }
}
