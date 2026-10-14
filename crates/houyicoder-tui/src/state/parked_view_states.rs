//! The open-row sets and the views they park for. The visible view's four
//! sets live as one value on the transcript view state; a transition parks
//! what the leaving view had open and installs what the entering view owns.
//! Keys are not globally unique, so every parked entry stays scoped to the
//! view that owns it: sessions park by session id, children by child id, and
//! the parent's sets wait in a single held slot while a child view is open.

use std::collections::HashSet;
use std::hash::Hash;

use houyicoder_protocol::frontend::SessionId;

use super::App;
use crate::row_id::RowId;
use crate::scroll::TranscriptScroll;

/// The four expansion sets one view owns.
#[derive(Debug, Default)]
pub(crate) struct ExpandedKeys {
    pub(crate) results: HashSet<String>,
    pub(crate) fold_groups: HashSet<String>,
    pub(crate) thinking: HashSet<RowId>,
    pub(crate) subagents: HashSet<String>,
}

impl ExpandedKeys {
    fn is_empty(&self) -> bool {
        self.results.is_empty()
            && self.fold_groups.is_empty()
            && self.thinking.is_empty()
            && self.subagents.is_empty()
    }

    /// Toggle one tool-result key. Returns true when the call changed a set,
    /// so the caller can tell a real toggle from a no-op.
    pub(crate) fn toggle_result(&mut self, key: String) -> bool {
        toggle_set(&mut self.results, key)
    }

    /// Toggle one fold-group key. Returns true when the call changed a set.
    pub(crate) fn toggle_fold(&mut self, key: String) -> bool {
        toggle_set(&mut self.fold_groups, key)
    }

    /// Toggle one thinking-turn key. Returns true when the call changed a set.
    pub(crate) fn toggle_thinking(&mut self, key: RowId) -> bool {
        toggle_set(&mut self.thinking, key)
    }

    /// Toggle one delegation key. Returns true when the call changed a set.
    pub(crate) fn toggle_subagent(&mut self, key: String) -> bool {
        toggle_set(&mut self.subagents, key)
    }
}

/// Remove-else-insert: present leaves, absent joins. True when the value
/// moved, false only when the set already held the expected end state.
fn toggle_set<T: Hash + Eq>(set: &mut HashSet<T>, key: T) -> bool {
    if set.remove(&key) {
        true
    } else {
        set.insert(key)
    }
}

/// What one child view leaves behind on exit: the rows it had open and the
/// viewport its reader parked at. A re-entry restores both.
pub(crate) struct ChildViewState {
    pub(crate) keys: ExpandedKeys,
    pub(crate) scroll: TranscriptScroll,
}

/// State parked for views that are not on screen. Every list is capped: a
/// view left long ago is not returned to in practice, and an unbounded
/// list would grow with the visit count.
#[derive(Default)]
pub(crate) struct ParkedViewStates {
    /// Expansion sets parked by the session that owns them, newest last.
    sessions: Vec<(SessionId, ExpandedKeys)>,
    /// Child view states parked within the current session visit, newest last.
    children: Vec<(String, ChildViewState)>,
    /// The parent's sets, held while a child view is open. One slot: at most
    /// one child view is ever on, and a hop between children passes through
    /// the canonical exit before the next entry.
    parent_held: Option<ExpandedKeys>,
}

/// Sessions whose expansion sets stay parked, newest last.
const PARKED_SESSIONS: usize = 16;

/// Children whose view states stay parked, newest last.
const PARKED_CHILDREN: usize = 16;

impl ParkedViewStates {
    /// Park the sets of the session being left. A session with nothing open
    /// is not kept: there is nothing to restore. A repeat park of the same
    /// session replaces its entry, so the entry count follows the sessions
    /// visited, not the number of switches.
    pub(crate) fn park_session(&mut self, session: SessionId, mut keys: ExpandedKeys) {
        // Only the local thinking keys are dropped. A durable key names the
        // log entry that closed its turn, which a rebuilt session reads back
        // from the log, so it opens the same block the reader left open; a
        // local key is a frame position of the visit that raised it, and a
        // rebuilt session holds no row at that position to open.
        keys.thinking.retain(RowId::is_durable);
        if keys.is_empty() {
            return;
        }
        self.sessions.retain(|(parked, _)| parked != &session);
        self.sessions.push((session, keys));
        if self.sessions.len() > PARKED_SESSIONS {
            self.sessions.remove(0);
        }
    }

    /// Take the sets parked for a session, if it has any. The session being
    /// entered is the only caller; its sets are parked again when it is left.
    pub(crate) fn take_session(&mut self, session: &SessionId) -> Option<ExpandedKeys> {
        let at = self
            .sessions
            .iter()
            .position(|(parked, _)| parked == session)?;
        Some(self.sessions.remove(at).1)
    }

    /// Hold the parent's sets for the child view being entered, and give
    /// back what that child parked on its last exit, if anything. The caller
    /// exits an open child view before entering the next one, so the held
    /// slot is empty on entry.
    pub(crate) fn enter_child(
        &mut self,
        child_sid: &str,
        parent_keys: ExpandedKeys,
    ) -> Option<ChildViewState> {
        debug_assert!(
            self.parent_held.is_none(),
            "a child view exits before the next one enters"
        );
        self.parent_held = Some(parent_keys);
        let at = self.children.iter().position(|(sid, _)| sid == child_sid)?;
        Some(self.children.remove(at).1)
    }

    /// Park the leaving child's view state under its id and give the parent's
    /// held sets back. A repeat park of the same child replaces its entry.
    /// The child's thinking keys park with the rest, unlike a session's: a
    /// child transcript is fetched whole from the child's own durable log on
    /// every entry, so its turn counters restart identically and a restored
    /// key opens the same block the reader left open.
    pub(crate) fn exit_child(
        &mut self,
        child_sid: &str,
        state: ChildViewState,
    ) -> Option<ExpandedKeys> {
        self.children.retain(|(sid, _)| sid != child_sid);
        self.children.push((child_sid.to_string(), state));
        if self.children.len() > PARKED_CHILDREN {
            self.children.remove(0);
        }
        self.parent_held.take()
    }

    /// Drop every parked child view state and any held parent sets. A session
    /// switch runs this: the new session's fleet is unrelated to the old
    /// one's children, and a child id can repeat across sessions, so a
    /// parked entry could otherwise restore into a different child's view.
    pub(crate) fn clear_children(&mut self) {
        self.children.clear();
        self.parent_held = None;
    }
}

impl App {
    /// Take the visible view's expansion sets as one value, leaving it empty.
    pub(crate) fn take_expanded_keys(&mut self) -> ExpandedKeys {
        self.ui.transcript_view.take_expanded()
    }

    /// Install a view's expansion sets as the visible ones, replacing the
    /// current value whole.
    pub(crate) fn set_expanded_keys(&mut self, keys: ExpandedKeys) {
        self.ui.transcript_view.swap_expanded(keys);
    }
}

#[cfg(test)]
mod tests;
