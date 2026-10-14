use super::*;

/// The two forms never collide: an entry identity and a frame count name
/// different coordinate spaces, so a cursor holding each stays distinct
/// even when the numbers inside them look alike.
#[test]
fn test_cursor_anchor_distinct() {
    let durable = EventCursor::Durable("01ARZ3NDEKTSV4RRFFQ69G5FAV".into());
    let local = EventCursor::Local(7);
    assert_ne!(durable, local);
}

/// A copied cursor equals its original, so a copy held past the state it
/// came from still names the same frame.
#[test]
fn test_cursor_clone_equal() {
    let a = EventCursor::Durable("01ARZ3NDEKTSV4RRFFQ69G5FAV".into());
    let b = a.clone();
    assert_eq!(a, b);
}
