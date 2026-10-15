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
fn test_park_filters_thinking_keys() {
    let mut parked = ParkedViewStates::default();
    let mut thinking_only = ExpandedKeys::default();
    thinking_only.thinking.insert(RowId::local(1));
    parked.park_session(sid("a"), thinking_only);
    assert!(
        parked.take_session(&sid("a")).is_none(),
        "a local thinking key alone leaves nothing to restore"
    );

    let mut mixed = ExpandedKeys::default();
    mixed.thinking.insert(RowId::local(1));
    mixed.thinking.insert("entry".into());
    mixed.results.insert("call-1".into());
    parked.park_session(sid("b"), mixed);
    let taken = parked.take_session(&sid("b")).expect("parked");
    assert!(taken.results.contains("call-1"));
    assert!(
        taken.thinking.contains(&"entry".into()),
        "the durable key names a log entry the rebuilt session reads back"
    );
    assert!(
        !taken.thinking.contains(&RowId::local(1)),
        "the local key stays behind: a rebuilt session has no row there"
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
