use super::ToolRuntime;
use crate::state::BashProgress;

/// A tick the runtime tracks moves the revision. The slot cache keys on it,
/// so a revision that stayed put would leave the previous chip text on
/// screen.
#[test]
fn test_tick_moves_revision() {
    let mut tools = ToolRuntime::default();
    tools.begin("c1".into());
    let before = tools.revision();

    tools.note_tick(
        "c1".into(),
        BashProgress {
            elapsed_secs: 3,
            ..Default::default()
        },
    );
    assert_eq!(tools.revision(), before + 1);
    assert_eq!(tools.tick("c1").map(|p| p.elapsed_secs), Some(3));

    tools.note_tick(
        "c1".into(),
        BashProgress {
            elapsed_secs: 4,
            tail: vec!["built 2 targets".into()],
            ..Default::default()
        },
    );
    assert_eq!(tools.revision(), before + 2);
    assert_eq!(
        tools.tick("c1").map(|p| p.tail.clone()),
        Some(vec!["built 2 targets".to_string()]),
        "the later tick replaces the earlier one"
    );
}

/// A tick for a call the runtime no longer tracks is dropped rather than
/// stored, so an out-of-order frame cannot resurrect a finished chip or
/// force a rebuild of rows it would not change.
#[test]
fn test_untracked_tick_dropped() {
    let mut tools = ToolRuntime::default();
    let before = tools.revision();

    tools.note_tick(
        "gone".into(),
        BashProgress {
            elapsed_secs: 9,
            ..Default::default()
        },
    );

    assert!(tools.tick("gone").is_none());
    assert_eq!(tools.revision(), before, "a dropped tick is not a change");
}

/// Finishing a call drops its ticker and moves the revision; finishing one
/// that was never running is not a change, so a duplicate result frame
/// cannot force a rebuild.
#[test]
fn test_finish_moves_revision() {
    let mut tools = ToolRuntime::default();
    tools.begin("c1".into());
    tools.note_tick(
        "c1".into(),
        BashProgress {
            elapsed_secs: 3,
            ..Default::default()
        },
    );
    let ticked = tools.revision();

    assert!(tools.finish("c1"), "the call was running");
    assert_eq!(tools.revision(), ticked + 1);
    assert!(tools.tick("c1").is_none());
    assert!(!tools.any_running());

    assert!(!tools.finish("c1"), "already finished");
    assert_eq!(
        tools.revision(),
        ticked + 1,
        "a duplicate result is not a change"
    );
}
