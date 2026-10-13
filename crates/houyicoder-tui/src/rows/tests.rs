use super::display_rows_cache::DisplayRowsCache;
use super::last_row_keys::LastRowKeys;
use super::row_set::{Row, RowSet};
use crate::row_id::RowId;
use crate::selection::TAG_PLAIN;

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

/// The cache publishes the rows and the version they were built at in one
/// operation, so a reader never sees one build's rows under another's
/// version.
#[test]
fn test_cache_publishes_one_build() {
    let cache = DisplayRowsCache::default();
    cache.replace(7, set_of(4, 0));
    assert_eq!(cache.version(), 7);
    assert_eq!(cache.with(|set| set.len()), 4);

    cache.replace(9, set_of(1, 0));
    assert_eq!(cache.version(), 9);
    assert_eq!(cache.with(|set| set.len()), 1);

    cache.reset();
    assert_eq!(cache.version(), u64::MAX);
    assert_eq!(cache.with(|set| set.len()), 0);
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
