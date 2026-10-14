use super::*;

/// next_selection snaps None to 0 and clamps at the bounds without
/// wrapping, including the empty-fleet guard.
#[test]
fn test_selection_clamps_at_bounds() {
    assert_eq!(next_selection(None, 3, 1), 0);
    assert_eq!(next_selection(Some(2), 3, 1), 2);
    assert_eq!(next_selection(Some(0), 3, -1), 0);
    assert_eq!(next_selection(None, 3, -1), 0);
    assert_eq!(next_selection(None, 0, 1), 0);
}
