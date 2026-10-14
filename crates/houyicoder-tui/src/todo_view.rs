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
//! last checklist instead of clearing it. A cold replay, idle replay or
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

/// Derived checklist state and its completion visibility lifecycle.
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
        let (start, reset) = match &self.cursor {
            None => (0, false),
            Some(EventCursor::Local(n)) => {
                let n = *n as usize;
                if n > abs_count {
                    (0, true)
                } else {
                    (n.saturating_sub(abs_base), false)
                }
            }
            // The generic slice carries no row identity; re-derive.
            Some(EventCursor::Durable(_)) => (0, true),
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
        if self.replaying_history
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
        if !self.replaying_history {
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
        if !self.replaying_history
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
    /// clears as a defensive fallback; cold replays normally clear in
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
        match &self.cursor {
            Some(EventCursor::Local(n)) => *n as usize,
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
mod tests;
