use super::*;

/// Every effective toggle bumps the presentation revision once; a toggle
/// that lands where the set already was (close_fold on a closed group)
/// does not. The revision is the render cache's input, so a missed bump
/// is a stale frame and a false bump is a wasted recompute.
#[test]
fn test_toggle_bumps_presentation() {
    let mut view = TranscriptViewState::default();
    let before = view.presentation_revision();
    view.toggle_fold("group-a".into());
    assert_eq!(view.presentation_revision(), before + 1, "a fold opens");
    view.toggle_fold("group-a".into());
    assert_eq!(
        view.presentation_revision(),
        before + 2,
        "the same fold closes"
    );
    view.close_fold("group-b");
    assert_eq!(
        view.presentation_revision(),
        before + 2,
        "closing a closed group is not a presentation change"
    );
    assert!(!view.fold_is_open("group-a"), "the toggle closed it");
}

/// A park or restore bumps only when the visible open rows actually
/// changed: restoring the same sets a view parked must not read as a
/// new presentation, or the cache recomputes a frame that did not move.
#[test]
fn test_swap_bumps_on_change() {
    let mut view = TranscriptViewState::default();
    view.toggle_result("call-1".into());
    let parked = view.take_expanded();
    let after_park = view.presentation_revision();

    view.swap_expanded(parked);
    assert_eq!(
        view.presentation_revision(),
        after_park + 1,
        "restoring open rows is a visible change from the empty state"
    );

    let same = ExpandedKeys {
        results: ["call-1".to_string()].into_iter().collect(),
        ..Default::default()
    };
    view.swap_expanded(same);
    assert_eq!(
        view.presentation_revision(),
        after_park + 1,
        "identical sets are not a presentation change"
    );

    view.swap_expanded(ExpandedKeys::default());
    let back_to_empty = view.presentation_revision();
    let empty = view.take_expanded();
    assert!(empty.results.is_empty());
    assert_eq!(
        view.presentation_revision(),
        back_to_empty,
        "parking nothing open is not a presentation change"
    );
}
