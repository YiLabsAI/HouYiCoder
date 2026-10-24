//! State and transitions for the /agents pane.
//!
//! One cursor walks the pane's three sections and one open detail sits over
//! the list. The cursor belongs to the pane: a running row mirrors the footer
//! strip's selection so the two highlights agree, but the pane never reads the
//! strip to decide which row is highlighted.

use std::cell::Cell;

/// The open type detail: which type, and the scroll position the view clamps.
/// The definition itself is resolved from the agent capability by name at
/// render time, so a refresh that drops the type closes the detail instead of
/// showing a stale copy.
pub(crate) struct AgentDetail {
    subagent_type: String,
    child_sid: Option<String>,
    offset: Cell<u16>,
    max_offset: Cell<u16>,
}

impl AgentDetail {
    pub(crate) fn subagent_type(&self) -> &str {
        &self.subagent_type
    }

    pub(crate) fn child_sid(&self) -> Option<&str> {
        self.child_sid.as_deref()
    }
}

/// List cursor plus the open detail for the /agents pane.
#[derive(Default)]
pub(crate) struct AgentsPaneState {
    cursor: Cell<usize>,
    detail: Option<AgentDetail>,
}

impl AgentsPaneState {
    pub(crate) fn cursor(&self) -> usize {
        self.cursor.get()
    }

    /// Move the cursor by a signed delta, clamped to the list bounds. A no-op
    /// on an empty list, where there is no row to point at.
    pub(crate) fn move_cursor(&self, delta: i32, len: usize) {
        if len == 0 {
            self.cursor.set(0);
            return;
        }
        let current = self.cursor.get().min(len - 1) as i32;
        self.cursor
            .set((current + delta).clamp(0, (len - 1) as i32) as usize);
    }

    /// Pull the cursor back inside a list of this length, so a rebuild that
    /// shortened or emptied the list never leaves it pointing past the end.
    pub(crate) fn clamp_cursor(&self, len: usize) {
        if len == 0 {
            self.cursor.set(0);
            return;
        }
        if self.cursor.get() >= len {
            self.cursor.set(len - 1);
        }
    }

    pub(crate) fn detail(&self) -> Option<&AgentDetail> {
        self.detail.as_ref()
    }

    /// The offset the open detail is scrolled to, zero when no detail is
    /// open. The render clamps it against the wrapped row count it measured.
    pub(crate) fn detail_offset(&self) -> u16 {
        self.detail
            .as_ref()
            .map(|detail| detail.offset.get())
            .unwrap_or(0)
    }

    /// Open the detail for one type at its first line, so a type opened after
    /// a scrolled one does not inherit the earlier position.
    pub(crate) fn open_detail(&mut self, subagent_type: String) {
        self.detail = Some(AgentDetail {
            subagent_type,
            child_sid: None,
            offset: Cell::new(0),
            max_offset: Cell::new(0),
        });
    }

    /// Open the in-pane detail for a running or completed child delegation.
    pub(crate) fn open_child_detail(&mut self, subagent_type: String, child_sid: String) {
        self.detail = Some(AgentDetail {
            subagent_type,
            child_sid: Some(child_sid),
            offset: Cell::new(0),
            max_offset: Cell::new(0),
        });
    }

    pub(crate) fn close_detail(&mut self) {
        self.detail = None;
    }

    /// Record the largest scroll offset the last frame needed, clamping the
    /// current offset so a shorter body cannot leave the view past its end.
    pub(crate) fn set_detail_max_offset(&self, max_offset: u16) {
        if let Some(detail) = self.detail.as_ref() {
            detail.max_offset.set(max_offset);
            detail.offset.set(detail.offset.get().min(max_offset));
        }
    }

    pub(crate) fn scroll_detail(&self, delta: i16) {
        let Some(detail) = self.detail.as_ref() else {
            return;
        };
        let next = if delta.is_negative() {
            detail.offset.get().saturating_sub(delta.unsigned_abs())
        } else {
            detail.offset.get().saturating_add(delta.unsigned_abs())
        };
        detail.offset.set(next.min(detail.max_offset.get()));
    }
}

#[cfg(test)]
mod tests;
