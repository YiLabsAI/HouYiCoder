//! One rendered transcript row and the sink that collects rows.
//!
//! The draw pass publishes six arrays that consumers read by row index, so a
//! row missing from one of them shifts every later row's metadata onto the
//! wrong text -- a click or key then acts on something other than what it
//! points at. Building a row as one value keeps that unrepresentable.

use ratatui::text::Line;

use crate::records::ToolOutcome;

/// A row's text plus the metadata a later click or key resolves by row index.
pub(super) struct Row {
    tag: u8,
    text: String,
    outcome: Option<ToolOutcome>,
    callid: Option<String>,
    fold_key: Option<String>,
    group: Option<String>,
    turn_id: Option<String>,
    pre: Option<Line<'static>>,
}

impl Row {
    pub(super) fn new(tag: u8, text: impl Into<String>) -> Self {
        Self {
            tag,
            text: text.into(),
            outcome: None,
            callid: None,
            fold_key: None,
            group: None,
            turn_id: None,
            pre: None,
        }
    }

    /// The blank spacer between sections. Plain by construction: a content
    /// tag would drag that tag's background across the gap.
    pub(super) fn spacer() -> Self {
        Self::new(crate::selection::TAG_PLAIN, String::new())
    }

    pub(super) fn outcome(mut self, outcome: Option<ToolOutcome>) -> Self {
        self.outcome = outcome;
        self
    }

    /// The result call id, so Ctrl+O on this row expands that result.
    pub(super) fn callid(mut self, callid: Option<String>) -> Self {
        self.callid = callid;
        self
    }

    /// The group key or child session id this row is an expand handle for.
    pub(super) fn fold_key(mut self, key: Option<String>) -> Self {
        self.fold_key = key;
        self
    }

    /// The expanded group this row sits inside, which paints the block
    /// background across the region.
    pub(super) fn group(mut self, group: Option<String>) -> Self {
        self.group = group;
        self
    }

    /// The reasoning turn id, set on a thought header so a click resolves
    /// straight to the turn rather than counting rows.
    pub(super) fn turn_id(mut self, turn_id: Option<String>) -> Self {
        self.turn_id = turn_id;
        self
    }

    /// A styled line, for styling the tag cannot express (markdown, diffs,
    /// badge colors).
    pub(super) fn pre(mut self, pre: Option<Line<'static>>) -> Self {
        self.pre = pre;
        self
    }
}

/// The arrays a draw pass publishes, in the order its consumers read them.
pub(super) type RowParts = (
    Vec<(u8, String, Option<ToolOutcome>)>,
    Vec<Option<String>>,
    Vec<Option<String>>,
    Vec<Option<String>>,
    Vec<Option<String>>,
    Vec<Option<Line<'static>>>,
);

/// Collects rows into the arrays the draw pass publishes.
#[derive(Default)]
pub(super) struct RowSink {
    rows: Vec<(u8, String, Option<ToolOutcome>)>,
    callids: Vec<Option<String>>,
    fold_keys: Vec<Option<String>>,
    groups: Vec<Option<String>>,
    turn_ids: Vec<Option<String>>,
    pre_rendered: Vec<Option<Line<'static>>>,
    in_subagent: bool,
}

impl RowSink {
    /// True while emitting the rows of an expanded delegation. Row builders
    /// read it to leave out their own expand affordances: a delegation's
    /// block is already one expanded thing, and every row inside advertising
    /// its own toggle turns a summary into a wall of hints nested one level
    /// deeper than the block the user opened.
    pub(super) fn in_subagent(&self) -> bool {
        self.in_subagent
    }

    /// Emit rows as the inside of a delegation. Saves and restores the flag
    /// rather than clearing it, so a nested delegation does not hand the
    /// outer one back its parent's context on the way out.
    pub(super) fn within_subagent<R>(&mut self, emit: impl FnOnce(&mut Self) -> R) -> R {
        let outer = self.in_subagent;
        self.in_subagent = true;
        let out = emit(self);
        self.in_subagent = outer;
        out
    }

    pub(super) fn push(&mut self, row: Row) {
        self.rows.push((row.tag, row.text, row.outcome));
        self.callids.push(row.callid);
        self.fold_keys.push(row.fold_key);
        self.groups.push(row.group);
        self.turn_ids.push(row.turn_id);
        self.pre_rendered.push(row.pre);
    }

    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows as plain text pairs, for the copy buffer.
    pub(super) fn text_rows(&self) -> Vec<(u8, String)> {
        self.rows.iter().map(|(t, s, _)| (*t, s.clone())).collect()
    }

    pub(super) fn into_parts(self) -> RowParts {
        (
            self.rows,
            self.callids,
            self.fold_keys,
            self.groups,
            self.turn_ids,
            self.pre_rendered,
        )
    }
}
