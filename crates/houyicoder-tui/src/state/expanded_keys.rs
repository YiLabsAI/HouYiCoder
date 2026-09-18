//! Expansion sets and the session that owns them. The App holds the open
//! session's four sets flat; a switch parks them under the session it leaves
//! and installs the entry for the session it enters, so leaving a session and
//! coming back keeps what the user had open. Keys are not globally unique (a
//! call id repeats across sessions), so the sets are kept apart by session id,
//! and the one key a rebuilt session re-mints is not parked at all.

use std::collections::HashSet;

use houyicoder_protocol::frontend::SessionId;

use super::App;

/// The four expansion sets one session owns.
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

/// Expansion sets parked by the session that owns them, so a switch away and
/// back restores what the user had open. A process that visits many sessions
/// holds only the most recent few: past PARKED_SESSIONS the oldest entry
/// drops, since a session the user has left that long ago is not returned to
/// in practice, and an unbounded map would grow with the session count.
#[derive(Default)]
pub(crate) struct ParkedKeys {
    entries: Vec<(SessionId, ExpandedKeys)>,
}

/// Sessions whose expansion sets stay parked, newest last.
const PARKED_SESSIONS: usize = 16;

impl ParkedKeys {
    /// Park the sets of the session being left. A session with nothing open
    /// is not kept: there is nothing to restore. A repeat park of the same
    /// session replaces its entry, so the entry count follows the sessions
    /// visited, not the number of switches.
    pub(crate) fn park(&mut self, session: SessionId, mut keys: ExpandedKeys) {
        // The thinking set is dropped rather than parked. Its key is a turn
        // counter that restarts when a session is rebuilt, so a restored key
        // would open a block of the new visit rather than the one the user
        // had open, and the rows it names are not replayed, so no key can
        // reach them anyway.
        keys.thinking.clear();
        if keys.is_empty() {
            return;
        }
        self.entries.retain(|(parked, _)| parked != &session);
        self.entries.push((session, keys));
        if self.entries.len() > PARKED_SESSIONS {
            self.entries.remove(0);
        }
    }

    /// Take the sets parked for a session, if it has any. The session being
    /// entered is the only caller; its sets are parked again when it is left.
    pub(crate) fn take_parked(&mut self, session: &SessionId) -> Option<ExpandedKeys> {
        let at = self
            .entries
            .iter()
            .position(|(parked, _)| parked == session)?;
        Some(self.entries.remove(at).1)
    }
}

impl App {
    /// Take the open session's expansion sets, leaving the four empty.
    pub(crate) fn take_expanded_keys(&mut self) -> ExpandedKeys {
        ExpandedKeys {
            results: std::mem::take(&mut self.expanded_results),
            fold_groups: std::mem::take(&mut self.expanded_fold_groups),
            thinking: std::mem::take(&mut self.expanded_thinking),
            subagents: std::mem::take(&mut self.expanded_subagents),
        }
    }

    /// Install a session's expansion sets as the open ones.
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

    #[test]
    fn test_park_only_for_owner() {
        let mut parked = ParkedKeys::default();
        parked.park(sid("a"), keys("one"));
        parked.park(sid("b"), keys("two"));
        assert!(parked.take_parked(&sid("c")).is_none(), "unknown session");
        let taken = parked.take_parked(&sid("a")).expect("session a is parked");
        assert!(taken.results.contains("one"));
        assert!(
            parked.take_parked(&sid("a")).is_none(),
            "a taken entry is gone; it comes back when the session is left again"
        );
        assert!(parked.take_parked(&sid("b")).is_some());
    }

    #[test]
    fn test_park_keeps_nothing_open() {
        let mut parked = ParkedKeys::default();
        parked.park(sid("a"), ExpandedKeys::default());
        assert!(
            parked.take_parked(&sid("a")).is_none(),
            "a session with no open keys has nothing to restore"
        );
    }

    #[test]
    fn test_park_drops_oldest() {
        let mut parked = ParkedKeys::default();
        for i in 0..PARKED_SESSIONS + 1 {
            parked.park(sid(&format!("s{i}")), keys("open"));
        }
        assert!(
            parked.take_parked(&sid("s0")).is_none(),
            "the oldest entry drops past the cap"
        );
        for i in 1..PARKED_SESSIONS + 1 {
            assert!(
                parked.take_parked(&sid(&format!("s{i}"))).is_some(),
                "s{i} stays within the cap"
            );
        }
    }

    #[test]
    fn test_park_drops_thinking_key() {
        let mut parked = ParkedKeys::default();
        let mut thinking_only = ExpandedKeys::default();
        thinking_only.thinking.insert("1".into());
        parked.park(sid("a"), thinking_only);
        assert!(
            parked.take_parked(&sid("a")).is_none(),
            "a thinking key alone leaves nothing to restore"
        );

        let mut mixed = ExpandedKeys::default();
        mixed.thinking.insert("1".into());
        mixed.results.insert("call-1".into());
        parked.park(sid("b"), mixed);
        let taken = parked.take_parked(&sid("b")).expect("parked");
        assert!(taken.results.contains("call-1"));
        assert!(
            taken.thinking.is_empty(),
            "the thinking key stays behind: {:?}",
            taken.thinking
        );
    }

    #[test]
    fn test_repeat_park_replaces_entry() {
        let mut parked = ParkedKeys::default();
        parked.park(sid("a"), keys("first"));
        parked.park(sid("a"), keys("second"));
        let taken = parked.take_parked(&sid("a")).expect("parked");
        assert!(
            taken.results.contains("second") && !taken.results.contains("first"),
            "the later sets replace the older ones for the same session"
        );
        assert!(
            parked.take_parked(&sid("a")).is_none(),
            "one entry, not two"
        );
    }
}
