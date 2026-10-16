use super::*;

use crate::scroll::ScrollAnchor;
use houyicoder_protocol::envelope::DurableEventId;

fn keys(value: &str) -> ExpandedKeys {
    ExpandedKeys {
        results: HashSet::from([value.to_string()]),
        ..Default::default()
    }
}

fn sid(value: &str) -> SessionId {
    SessionId(value.to_string())
}

fn view_state(value: &str) -> ParkedViewState {
    ParkedViewState {
        keys: keys(value),
        scroll: TranscriptScroll::default(),
    }
}

/// A viewport parked on this row, with the place's own offset left at the
/// start of the row.
fn parked_scroll(row: RowId) -> TranscriptScroll {
    let mut scroll = TranscriptScroll::default();
    scroll.park(ScrollAnchor {
        row,
        char_offset: 0,
    });
    scroll
}

/// A row the log named: the identity a rebuilt session reads back.
fn durable_row(event: &str) -> RowId {
    RowId::Durable {
        event: DurableEventId(event.to_string()),
        slot: 0,
    }
}

#[test]
fn test_park_only_for_owner() {
    let mut parked = ParkedViewStates::default();
    parked.park_session(sid("a"), view_state("one"));
    parked.park_session(sid("b"), view_state("two"));
    assert!(parked.take_session(&sid("c")).is_none(), "unknown session");
    let taken = parked.take_session(&sid("a")).expect("session a is parked");
    assert!(taken.keys.results.contains("one"));
    assert!(
        parked.take_session(&sid("a")).is_none(),
        "a taken entry is gone; it comes back when the session is left again"
    );
    assert!(parked.take_session(&sid("b")).is_some());
}

#[test]
fn test_park_skips_empty() {
    let mut parked = ParkedViewStates::default();
    parked.park_session(sid("a"), ParkedViewState::default());
    assert!(
        parked.take_session(&sid("a")).is_none(),
        "a session with nothing open and its reader at the tail has nothing \
         to restore"
    );
}

#[test]
fn test_park_drops_oldest() {
    let mut parked = ParkedViewStates::default();
    for i in 0..PARKED_SESSIONS + 1 {
        parked.park_session(sid(&format!("s{i}")), view_state("open"));
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
fn test_park_filters_thinking_keys() {
    let mut parked = ParkedViewStates::default();
    let mut thinking_only = ExpandedKeys::default();
    thinking_only.thinking.insert(RowId::local(1));
    parked.park_session(
        sid("a"),
        ParkedViewState {
            keys: thinking_only,
            scroll: TranscriptScroll::default(),
        },
    );
    assert!(
        parked.take_session(&sid("a")).is_none(),
        "a local thinking key alone leaves nothing to restore"
    );

    let mut mixed = ExpandedKeys::default();
    mixed.thinking.insert(RowId::local(1));
    mixed.thinking.insert("entry".into());
    mixed.results.insert("call-1".into());
    parked.park_session(
        sid("b"),
        ParkedViewState {
            keys: mixed,
            scroll: TranscriptScroll::default(),
        },
    );
    let taken = parked.take_session(&sid("b")).expect("parked");
    assert!(taken.keys.results.contains("call-1"));
    assert!(
        taken.keys.thinking.contains(&"entry".into()),
        "the durable key names a log entry the rebuilt session reads back"
    );
    assert!(
        !taken.keys.thinking.contains(&RowId::local(1)),
        "the local key stays behind: a rebuilt session has no row there"
    );
}

#[test]
fn test_repeat_park_replaces_entry() {
    let mut parked = ParkedViewStates::default();
    parked.park_session(sid("a"), view_state("first"));
    parked.park_session(sid("a"), view_state("second"));
    let taken = parked.take_session(&sid("a")).expect("parked");
    assert!(
        taken.keys.results.contains("second") && !taken.keys.results.contains("first"),
        "the later sets replace the older ones for the same session"
    );
    assert!(
        parked.take_session(&sid("a")).is_none(),
        "one entry, not two"
    );
}

/// A reader who rolled back to a row the log named finds the same place when
/// the session is entered again: the rebuilt view reads the same entry, so
/// the identity still names that row.
#[test]
fn test_session_restores_place() {
    let mut parked = ParkedViewStates::default();
    parked.park_session(
        sid("a"),
        ParkedViewState {
            keys: ExpandedKeys::default(),
            scroll: parked_scroll(durable_row("e7")),
        },
    );
    let taken = parked
        .take_session(&sid("a"))
        .expect("a parked place is something to restore");
    let anchor = taken.scroll.anchor().expect("the place comes back");
    assert_eq!(
        anchor.row,
        durable_row("e7"),
        "the viewport stands on the row the reader left it on"
    );
}

/// A place taken on a frame position is given up at the park instead of
/// resolved in the session entered: that position holds a different row
/// there, and landing the reader on an unrelated row is worse than not
/// landing them at all.
#[test]
fn test_local_anchor_dropped() {
    let mut parked = ParkedViewStates::default();
    parked.park_session(
        sid("a"),
        ParkedViewState {
            keys: ExpandedKeys::default(),
            scroll: parked_scroll(RowId::local(3)),
        },
    );
    assert!(
        parked.take_session(&sid("a")).is_none(),
        "a frame position alone leaves nothing the session entered can hold"
    );

    parked.park_session(
        sid("b"),
        ParkedViewState {
            keys: keys("open"),
            scroll: parked_scroll(RowId::local(3)),
        },
    );
    let taken = parked.take_session(&sid("b")).expect("parked");
    assert!(
        taken.scroll.is_following_tail(),
        "the local place is the half that is given up"
    );
    assert!(
        taken.keys.results.contains("open"),
        "the durable half is kept"
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
    parked.sessions.push((sid("s"), view_state("session")));
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
