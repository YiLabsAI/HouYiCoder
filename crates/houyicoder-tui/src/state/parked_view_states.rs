//! The open-row sets and the views they park for. The App holds the
//! visible view's four sets flat; a transition parks what the leaving
//! view had open and installs what the entering view owns. Keys are
//! not globally unique, so every parked entry stays scoped to the view
//! that owns it: sessions park by session id, children by child id, and the
//! parent's sets wait in a single held slot while a child view is open.

use std::collections::HashSet;

use houyicoder_protocol::frontend::SessionId;

use super::App;
use crate::scroll::TranscriptScroll;

/// The four expansion sets one view owns.
#[derive(Default)]
pub(crate) struct ExpandedKeys {
    pub(crate) results: HashSet<String>,
    pub(crate) fold_groups: HashSet<String>,
    pub(crate) thinking: HashSet<String>,
    pub(crate) subagents: HashSet<String>,
}

impl ExpandedKeys {
    fn is_empty(&self) -> bool {
        self.results.is_empty()
            && self.fold_groups.is_empty()
            && self.thinking.is_empty()
            && self.subagents.is_empty()
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
        // The thinking set is dropped rather than parked. Its key is a turn
        // counter that restarts when a session is rebuilt, so a restored key
        // would open a block of the new visit rather than the one the user
        // had open, and the rows it names are not replayed, so no key can
        // reach them anyway.
        keys.thinking.clear();
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
    /// Take the visible view's expansion sets, leaving the four empty.
    pub(crate) fn take_expanded_keys(&mut self) -> ExpandedKeys {
        ExpandedKeys {
            results: std::mem::take(&mut self.expanded_results),
            fold_groups: std::mem::take(&mut self.expanded_fold_groups),
            thinking: std::mem::take(&mut self.expanded_thinking),
            subagents: std::mem::take(&mut self.expanded_subagents),
        }
    }

    /// Install a view's expansion sets as the visible ones.
    pub(crate) fn set_expanded_keys(&mut self, keys: ExpandedKeys) {
        self.expanded_results = keys.results;
        self.expanded_fold_groups = keys.fold_groups;
        self.expanded_thinking = keys.thinking;
        self.expanded_subagents = keys.subagents;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(value: &str) -> ExpandedKeys {
        ExpandedKeys {
            results: HashSet::from([value.to_string()]),
            ..Default::default()
        }
    }

    fn sid(value: &str) -> SessionId {
        SessionId(value.to_string())
    }

    fn view_state(value: &str) -> ChildViewState {
        ChildViewState {
            keys: keys(value),
            scroll: TranscriptScroll::default(),
        }
    }

    #[test]
    fn test_park_only_for_owner() {
        let mut parked = ParkedViewStates::default();
        parked.park_session(sid("a"), keys("one"));
        parked.park_session(sid("b"), keys("two"));
        assert!(parked.take_session(&sid("c")).is_none(), "unknown session");
        let taken = parked.take_session(&sid("a")).expect("session a is parked");
        assert!(taken.results.contains("one"));
        assert!(
            parked.take_session(&sid("a")).is_none(),
            "a taken entry is gone; it comes back when the session is left again"
        );
        assert!(parked.take_session(&sid("b")).is_some());
    }

    #[test]
    fn test_park_keeps_nothing_open() {
        let mut parked = ParkedViewStates::default();
        parked.park_session(sid("a"), ExpandedKeys::default());
        assert!(
            parked.take_session(&sid("a")).is_none(),
            "a session with no open keys has nothing to restore"
        );
    }

    #[test]
    fn test_park_drops_oldest() {
        let mut parked = ParkedViewStates::default();
        for i in 0..PARKED_SESSIONS + 1 {
            parked.park_session(sid(&format!("s{i}")), keys("open"));
        }
        assert!(
            parked.take_session(&sid("s0")).is_none(),
            "the oldest entry drops past the cap"
        );
        for i in 1..PARKED_SESSIONS + 1 {
            let want = sid(&format!("s{i}"));
            assert!(
                parked.take_session(&want).is_some(),
                "s{i} stays within the cap"
            );
        }
    }

    #[test]
    fn test_park_drops_thinking_key() {
        let mut parked = ParkedViewStates::default();
        let mut thinking_only = ExpandedKeys::default();
        thinking_only.thinking.insert("1".into());
        parked.park_session(sid("a"), thinking_only);
        assert!(
            parked.take_session(&sid("a")).is_none(),
            "a thinking key alone leaves nothing to restore"
        );

        let mut mixed = ExpandedKeys::default();
        mixed.thinking.insert("1".into());
        mixed.results.insert("call-1".into());
        parked.park_session(sid("b"), mixed);
        let taken = parked.take_session(&sid("b")).expect("parked");
        assert!(taken.results.contains("call-1"));
        assert!(
            taken.thinking.is_empty(),
            "the thinking key stays behind: {:?}",
            taken.thinking
        );
    }

    #[test]
    fn test_repeat_park_replaces_entry() {
        let mut parked = ParkedViewStates::default();
        parked.park_session(sid("a"), keys("first"));
        parked.park_session(sid("a"), keys("second"));
        let taken = parked.take_session(&sid("a")).expect("parked");
        assert!(
            taken.results.contains("second") && !taken.results.contains("first"),
            "the later sets replace the older ones for the same session"
        );
        assert!(
            parked.take_session(&sid("a")).is_none(),
            "one entry, not two"
        );
    }

    #[test]
    fn test_child_park_restores() {
        let mut parked = ParkedViewStates::default();
        // First entry: nothing parked for this child yet, and the parent's
        // sets go into the held slot.
        assert!(parked.enter_child("c1", keys("parent")).is_none());
        // Exit parks the child's own view state and gives the parent's back.
        let held = parked
            .exit_child("c1", view_state("child"))
            .expect("the parent's sets come back");
        assert!(held.results.contains("parent"));
        // Re-entry restores what the child left behind.
        let back = parked
            .enter_child("c1", keys("parent"))
            .expect("the child has a parked view state");
        assert!(back.keys.results.contains("child"));
        assert!(
            parked.exit_child("c1", view_state("child")).is_some(),
            "the held slot answers every exit"
        );
    }

    #[test]
    fn test_child_park_caps() {
        let mut parked = ParkedViewStates::default();
        for i in 0..PARKED_CHILDREN + 1 {
            parked.enter_child(&format!("c{i}"), ExpandedKeys::default());
            parked.exit_child(&format!("c{i}"), view_state("open"));
        }
        let newest = format!("c{}", PARKED_CHILDREN);
        assert!(
            parked
                .enter_child(&newest, ExpandedKeys::default())
                .is_some(),
            "the newest child stays within the cap"
        );
        parked.exit_child(&newest, view_state("open"));
        assert!(
            parked.enter_child("c0", ExpandedKeys::default()).is_none(),
            "the oldest child drops past the cap"
        );
        parked.exit_child("c0", view_state("open"));
    }

    #[test]
    fn test_clear_children() {
        let mut parked = ParkedViewStates::default();
        parked.enter_child("c1", keys("parent"));
        parked.exit_child("c1", view_state("child"));
        parked.sessions.push((sid("s"), keys("session")));
        // A child view open at clear time: its held parent sets must go too.
        parked.enter_child("c1", keys("parent"));
        parked.clear_children();
        assert!(
            parked.parent_held.is_none(),
            "a held parent slot does not survive the clear"
        );
        assert!(
            parked.enter_child("c1", ExpandedKeys::default()).is_none(),
            "the parked child is gone"
        );
        parked.exit_child("c1", view_state("open"));
        assert!(
            parked.take_session(&sid("s")).is_some(),
            "session parking is a different lifetime and stands"
        );
    }
}
