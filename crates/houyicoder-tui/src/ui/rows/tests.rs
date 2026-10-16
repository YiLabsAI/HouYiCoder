use super::display_rows_cache::DisplayRowsCache;
use super::last_row_keys::LastRowKeys;
use super::row_set::{Row, RowSet};
use super::slots_key::SlotsKey;
use crate::records::TeammateView;
use crate::row_id::RowId;
use crate::selection::TAG_PLAIN;
use crate::test_harness::working_app;

/// A row whose metadata names the row, so a window that gives a row its
/// neighbour's keys is visible in the assertion rather than only in length.
fn tagged_row(i: usize) -> Row {
    Row::new(TAG_PLAIN, format!("row {i}"))
        .callid(Some(format!("call {i}")))
        .fold_key(Some(format!("fold {i}")))
        .group(Some(format!("group {i}")))
        .turn_id(Some(RowId::from(format!("turn {i}").as_str())))
}

fn set_of(len: usize, offset: usize) -> RowSet {
    let mut set = RowSet::default();
    for i in 0..len {
        set.push(tagged_row(i + offset));
    }
    set
}

/// Every array the set publishes is as long as its rows, after pushes and
/// after a window. A metadata vector added to Row without a push would leave
/// this short, which is the desync a reader then resolves onto the wrong row.
#[test]
fn test_push_keeps_rows_together() {
    let set = set_of(5, 0);
    assert_eq!(set.len(), 5);
    assert_eq!(set.callids().len(), 5);
    assert_eq!(set.fold_keys().len(), 5);
    assert_eq!(set.expanded_group().len(), 5);
    assert_eq!(set.turn_ids().len(), 5);
    assert_eq!(set.pre_rendered().len(), 5);
}

/// A window over the boundary between the cached half and the live half
/// carries each row's own metadata. Slicing the halves separately is what let
/// the metadata of the row beside the one shown arrive instead.
#[test]
fn test_window_keeps_row_keys() {
    let slots = set_of(3, 0);
    let live = set_of(2, 100);
    let window = slots.visible(&live, 2, 3);

    let texts: Vec<&str> = window.rows().iter().map(|(_, s, _)| s.as_str()).collect();
    assert_eq!(texts, ["row 2", "row 100", "row 101"]);

    let callids: Vec<Option<&str>> = window.callids().iter().map(|c| c.as_deref()).collect();
    assert_eq!(
        callids,
        [Some("call 2"), Some("call 100"), Some("call 101")]
    );

    let folds: Vec<Option<&str>> = window.fold_keys().iter().map(|k| k.as_deref()).collect();
    assert_eq!(folds, [Some("fold 2"), Some("fold 100"), Some("fold 101")]);

    let turns: Vec<Option<RowId>> = window.turn_ids().to_vec();
    assert_eq!(
        turns,
        [
            Some(RowId::from("turn 2")),
            Some(RowId::from("turn 100")),
            Some(RowId::from("turn 101")),
        ]
    );
}

/// Every screen row the set holds names itself: the anchor taken at a row
/// resolves back to that row. The row just past the last named one is where the
/// viewport crosses into the live tail, and it is the row the two directions
/// disagree about -- its name is the last line's text plus no rows beyond, and
/// read back as one of the line's own rows it draws the reader one row off.
#[test]
fn test_anchor_names_row() {
    let mut set = RowSet::default();
    set.begin_line(RowId::local(3));
    set.push(Row::new(TAG_PLAIN, "first line"));
    set.push(Row::spacer());
    set.begin_line(RowId::local(4));
    set.push(Row::new(TAG_PLAIN, "second line"));
    set.push(Row::new(TAG_PLAIN, "second line wraps"));
    let held = set.len();

    for row in 0..=held + 2 {
        let anchor = set.anchor_at(row).expect("every row stands on a name");
        assert_eq!(
            set.row_of(&anchor),
            Some(row),
            "the anchor taken at row {row} resolved elsewhere"
        );
    }
}

/// A window never reports more rows than the two halves hold, and a request
/// past the end is empty rather than a short slice of one array.
#[test]
fn test_window_bounds() {
    let slots = set_of(3, 0);
    let live = set_of(2, 100);

    let past_end = slots.visible(&live, 9, 4);
    assert!(past_end.is_empty());
    assert_eq!(past_end.callids().len(), 0);

    let no_cap = slots.visible(&live, 0, 0);
    assert!(no_cap.is_empty());

    let whole = slots.visible(&live, 0, 99);
    assert_eq!(whole.len(), 5);
    assert_eq!(whole.callids().len(), 5);
    assert_eq!(whole.turn_ids().len(), 5);
}

/// The cache publishes the rows and the key they were built for in one
/// operation, so a reader never sees one build's rows under another's key.
#[test]
fn test_cache_publishes_one_build() {
    let app = working_app();
    let cache = DisplayRowsCache::default();
    let at_80 = SlotsKey::for_frame(&app, 80);
    let at_100 = SlotsKey::for_frame(&app, 100);

    assert!(
        !cache.holds(&at_80),
        "nothing is cached before the first build"
    );
    cache.replace(at_80.clone(), set_of(4, 0));
    assert!(cache.holds(&at_80));
    assert!(!cache.holds(&at_100), "a different key is not a hit");
    assert_eq!(cache.with(|set| set.len()), 4);

    cache.replace(at_100.clone(), set_of(1, 0));
    assert!(cache.holds(&at_100));
    assert_eq!(cache.with(|set| set.len()), 1);

    cache.reset();
    assert!(!cache.holds(&at_100), "a reset drops the key too");
    assert_eq!(cache.with(|set| set.len()), 0);
}

/// Every input the slot builder reads moves the key. A field the builder
/// reads but the key omits would serve the previous build's rows for a view
/// that changed.
#[test]
fn test_key_tracks_each_input() {
    let base = SlotsKey::for_frame(&working_app(), 80);

    assert_ne!(
        SlotsKey::for_frame(&working_app(), 100),
        base,
        "the wrap width"
    );

    let mut verbose = working_app();
    verbose.ui.verbose = true;
    assert_ne!(SlotsKey::for_frame(&verbose, 80), base, "verbose mode");

    let mut busy = working_app();
    busy.start_run_for_test(7);
    assert_ne!(SlotsKey::for_frame(&busy, 80), base, "an active run");

    // Waiting holds the same run but is not busy, and the fold groups read
    // busy to decide whether the trailing turn is still active.
    let mut waiting = working_app();
    waiting.start_run_for_test(7);
    waiting.sessions.current.run.begin_waiting();
    let waiting = SlotsKey::for_frame(&waiting, 80);
    assert_ne!(waiting, base, "a paused run");
    assert_ne!(
        SlotsKey::for_frame(&busy, 80),
        waiting,
        "busy against paused"
    );

    let typed = {
        let app = working_app();
        app.sessions.current.transcript.bump_revision();
        app
    };
    assert_ne!(SlotsKey::for_frame(&typed, 80), base, "a transcript edit");

    let mut expanded = working_app();
    expanded.ui.transcript_view.toggle_fold("group-a".into());
    assert_ne!(
        SlotsKey::for_frame(&expanded, 80),
        base,
        "an expansion change"
    );

    let mut child = working_app();
    child.ui.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });
    assert_ne!(SlotsKey::for_frame(&child, 80), base, "a child view");

    let mut search = working_app();
    search.enter_search_view("needle");
    assert_ne!(SlotsKey::for_frame(&search, 80), base, "the search corpus");
}

/// A bash tick moves the key at an unchanged width, because the elapsed and
/// line count are baked into the cached chip text. A revision that did not
/// advance on a tick would leave the previous chip on screen.
#[test]
fn test_key_tracks_a_tick() {
    let mut app = working_app();
    app.start_run_for_test(0);
    let unticked = SlotsKey::for_frame(&app, 80);

    let progress = app.run_progress_mut().expect("active run");
    progress.tools.begin("c1".into());
    progress.tools.note_tick("c1".into(), 3, None);

    assert_ne!(
        SlotsKey::for_frame(&app, 80),
        unticked,
        "a bash tick at the same width must still move the key"
    );
}

/// The wrap width and the bash tick revision are separate fields. Folded into
/// one number they merged: the tick set {call-1, call-2} at 80 columns and no
/// tick at 111 columns produced the same value, so the cache served rows
/// wrapped for a width the frame no longer had. The set is the one the design
/// records as colliding; two ticked calls is what makes the merge real.
#[test]
fn test_width_and_tick_apart() {
    let mut unticked = working_app();
    unticked.start_run_for_test(0);
    let at_111_no_tick = SlotsKey::for_frame(&unticked, 111);

    let mut ticked = working_app();
    ticked.start_run_for_test(0);
    {
        let progress = ticked.run_progress_mut().expect("active run");
        progress.tools.begin("call-1".into());
        progress.tools.begin("call-2".into());
        progress.tools.note_tick("call-1".into(), 0, None);
        progress.tools.note_tick("call-2".into(), 0, None);
    }
    let at_80_ticked = SlotsKey::for_frame(&ticked, 80);

    assert_ne!(
        at_80_ticked, at_111_no_tick,
        "a ticked 80-column frame must not read as an unticked 111-column one"
    );
}

/// The key set takes all four vectors from one row set, so a replacement
/// cannot leave one of them at the previous frame's length.
#[test]
fn test_keys_from_one_set() {
    let mut keys = LastRowKeys::default();
    keys.replace(&set_of(3, 0));
    assert_eq!(keys.fold_keys().len(), 3);
    assert_eq!(keys.turn_ids().len(), 3);
    assert_eq!(keys.fold_key_at(2).as_deref(), Some("fold 2"));
    assert_eq!(keys.turn_id_at(1), Some(RowId::from("turn 1")));

    // A shorter set must not leave the longer one's tail behind: the row the
    // tail belonged to is gone, so its key would resolve a click onto a row
    // that no longer exists.
    keys.replace(&set_of(1, 0));
    assert_eq!(keys.fold_keys().len(), 1);
    assert_eq!(keys.turn_ids().len(), 1);
    assert_eq!(keys.fold_key_at(1), None);
    assert_eq!(keys.turn_id_at(1), None);
}
