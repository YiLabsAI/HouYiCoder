//! The queue's own rules: which commands are state-changing, how the cursor
//! clamps, and what a commit, a drain, and a session carry do to the items.

use std::slice::from_ref;

use super::*;

#[test]
fn test_state_changing_set_narrow() {
    for c in [
        "clear",
        "resume",
        "resume sid",
        "rewind",
        "rewind plan",
        "undo",
    ] {
        assert!(is_state_changing(c), "{c:?} should be state-changing");
    }
}

#[test]
fn test_ui_commands_not_deferred() {
    for c in [
        "status",
        "model",
        "search foo",
        "tips",
        "hooks",
        "debug",
        "trajectory",
        "context",
        "sandbox",
        "cost",
        "graph",
        "diff",
        "agents",
        "memory",
        "help",
        "release-notes",
        "worktrees",
        "export",
        "permissions",
        "init",
        "plan",
        "exit",
    ] {
        assert!(!is_state_changing(c), "{c:?} should execute immediately");
    }
}

#[test]
fn test_command_display_keeps_slash() {
    let c = InputItem::Command("/resume sid-123".into());
    assert_eq!(c.display(), "/resume sid-123");
}

#[test]
fn test_message_display_returns_text() {
    let m = InputItem::Message("hello".into());
    assert_eq!(m.display(), "hello");
}

#[test]
fn test_cursor_clamps_to_bounds() {
    let mut q = InputQueue::default();
    q.move_cursor(1);
    assert_eq!(q.selected_index(), None, "an empty queue selects nothing");
    q.push(InputItem::Command("/a".into()));
    q.push(InputItem::Command("/b".into()));
    q.move_cursor(1);
    assert_eq!(
        q.selected_index(),
        Some(1),
        "down movement reaches the last row"
    );
    q.move_cursor(5);
    assert_eq!(
        q.selected_index(),
        Some(1),
        "down movement stops at the last row"
    );
    q.move_cursor(-5);
    assert_eq!(
        q.selected_index(),
        Some(0),
        "up movement stops at the first row"
    );
}

#[test]
fn test_resume_target_extracted() {
    let cmd = InputItem::Command("/resume sid-123".into());
    assert_eq!(resume_command_target(&cmd), Some("sid-123"));
}

#[test]
fn test_resume_path_keeps_spaces() {
    let cmd = InputItem::Command("/resume /tmp/my transcript.json".into());
    assert_eq!(
        resume_command_target(&cmd),
        Some("/tmp/my transcript.json"),
        "an export path with spaces survives intact, not truncated"
    );
}

#[test]
fn test_resume_target_rejects_others() {
    assert_eq!(
        resume_command_target(&InputItem::Command("/resume".into())),
        None,
        "a bare /resume has no target"
    );
    assert_eq!(
        resume_command_target(&InputItem::Command("/clear".into())),
        None,
        "a non-resume command has no target"
    );
    assert_eq!(
        resume_command_target(&InputItem::Message("hello".into())),
        None,
        "a message is not a command"
    );
}

#[test]
fn test_drain_resume_takes_head() {
    let mut q = InputQueue::default();
    q.push(InputItem::Message("tail".into()));
    q.insert(0, InputItem::Command("/resume sid-xyz".into()));
    assert_eq!(q.drain_resume_target(), Some("sid-xyz".to_string()));
    assert_eq!(q.len(), 1, "the head was removed, the tail stays");
}

#[test]
fn test_drain_skips_without_target() {
    let mut q = InputQueue::default();
    q.push(InputItem::Message("hello".into()));
    assert_eq!(
        q.drain_resume_target(),
        None,
        "a message head is not drained"
    );
    assert_eq!(q.len(), 1, "the queue is untouched");

    let mut bare = InputQueue::default();
    bare.push(InputItem::Command("/resume".into()));
    assert_eq!(
        bare.drain_resume_target(),
        None,
        "a bare /resume head has no target to drain"
    );
    assert_eq!(bare.len(), 1, "a bare /resume head stays queued");
}

#[test]
fn test_selected_index_bounds() {
    let mut q = InputQueue::default();
    assert_eq!(q.selected_index(), None, "an empty queue selects nothing");
    q.push(InputItem::Command("/a".into()));
    assert_eq!(q.selected_index(), Some(0), "a single item is selected");
}

#[test]
fn test_remove_clamps_cursor() {
    let mut q = InputQueue::default();
    q.push(InputItem::Command("/a".into()));
    q.push(InputItem::Command("/b".into()));
    q.move_cursor(1);
    assert_eq!(
        q.selected_index(),
        Some(1),
        "the cursor sits on the last row"
    );
    q.remove(1);
    assert_eq!(
        q.selected_index(),
        Some(0),
        "removing the selected last row clamps to the new last row"
    );
}

#[test]
fn test_replace_clamps_cursor() {
    let mut q = InputQueue::default();
    q.push(InputItem::Command("/a".into()));
    q.push(InputItem::Command("/b".into()));
    q.push(InputItem::Command("/c".into()));
    q.move_cursor(1);
    q.move_cursor(1);
    assert_eq!(
        q.selected_index(),
        Some(2),
        "the cursor sits on the last row"
    );
    q.replace_all(vec![InputItem::Command("/only".into())]);
    assert_eq!(
        q.selected_index(),
        Some(0),
        "a shorter replacement clamps the cursor into range"
    );
}

#[test]
fn test_take_resets_cursor() {
    let mut q = InputQueue::default();
    q.push(InputItem::Command("/a".into()));
    q.push(InputItem::Command("/b".into()));
    q.move_cursor(1);
    assert_eq!(
        q.selected_index(),
        Some(1),
        "the cursor sits on the last row"
    );
    let taken = q.take_all();
    assert_eq!(taken.len(), 2, "take_all hands out the whole queue");
    assert_eq!(q.selected_index(), None, "an emptied queue selects nothing");
    q.push(InputItem::Command("/c".into()));
    q.push(InputItem::Command("/d".into()));
    assert_eq!(
        q.selected_index(),
        Some(0),
        "the selection restarts at the head, not the stale row"
    );
}

/// A commit removes exactly the items whose stable identity matches, so a
/// delayed or repeated event cannot drop newer text at the same position.
#[test]
fn test_remove_committed() {
    let mut q = InputQueue::default();
    let kept = QueuedInput::new("kept");
    let gone = QueuedInput::new("gone");
    q.push(InputItem::Message(kept.clone()));
    q.push(InputItem::Message(gone.clone()));
    q.remove_committed(from_ref(&gone));
    assert_eq!(q.len(), 1, "the committed item leaves the queue");
    assert_eq!(
        q.first().unwrap().display(),
        "kept",
        "the uncommitted item stays"
    );
    q.remove_committed(from_ref(&gone));
    assert_eq!(q.len(), 1, "a repeated commit for the same id is a no-op");
    let absent = QueuedInput::new("absent");
    q.remove_committed(&[absent]);
    assert_eq!(q.len(), 1, "a commit for an unqueued id is a no-op");
    let parked = QueuedInput::new("parked");
    q.push(InputItem::ParkedMessage(parked.clone()));
    q.remove_committed(from_ref(&parked));
    assert_eq!(
        q.len(),
        1,
        "a committed parked item leaves the queue by the same identity"
    );
}

#[test]
fn test_demote_parks_messages() {
    let mut q = InputQueue::default();
    q.push(InputItem::Message("live".into()));
    q.push(InputItem::Command("/cmd".into()));
    q.demote_to_parked();
    assert!(
        matches!(q[0], InputItem::ParkedMessage(_)),
        "a live message becomes parked"
    );
    assert!(
        matches!(q[1], InputItem::Command(_)),
        "a command is left alone"
    );
}

/// The switch carry rules: both resume forms and messages cross, other
/// commands drop, carried messages park, and the selection resets to
/// the top. An empty queue transfers to nothing.
#[test]
fn test_transfer_carry_rules() {
    let mut q = InputQueue::default();
    assert!(
        q.transfer_across_sessions().is_empty(),
        "an empty queue has nothing to carry or report"
    );
    q.push(InputItem::Message("task".into()));
    q.push(InputItem::Command("/clear".into()));
    q.push(InputItem::Command("/resume sid-1".into()));
    q.push(InputItem::Command("/resume".into()));
    q.push(InputItem::ParkedMessage("held".into()));
    q.move_cursor(4);
    let dropped = q.transfer_across_sessions();
    assert_eq!(
        dropped,
        vec!["/clear".to_string()],
        "the dropped command is reported for the notice"
    );
    assert_eq!(
        q,
        vec![
            InputItem::ParkedMessage("task".into()),
            InputItem::Command("/resume sid-1".into()),
            InputItem::Command("/resume".into()),
            InputItem::ParkedMessage("held".into()),
        ],
        "messages cross parked, both resume forms cross, the rest drops"
    );
    assert_eq!(
        q.selected_index(),
        Some(0),
        "the old session's selection does not carry"
    );
}

#[test]
fn test_live_copy_only_message() {
    let mut q = InputQueue::default();
    let live = QueuedInput::new("live");
    let parked = QueuedInput::new("parked");
    q.push(InputItem::Message(live.clone()));
    q.push(InputItem::Command("/cmd".into()));
    q.push(InputItem::ParkedMessage(parked));
    assert_eq!(
        q.live_copy_at(0),
        Some(live.id),
        "a live message holds its runner id"
    );
    assert_eq!(q.live_copy_at(1), None, "a command holds no runner copy");
    assert_eq!(
        q.live_copy_at(2),
        None,
        "a parked message holds no runner copy"
    );
    assert_eq!(
        q.live_copy_at(3),
        None,
        "an out-of-range index holds nothing"
    );
}

#[test]
fn test_confirm_head_promotes_parked() {
    let mut q = InputQueue::default();
    q.push(InputItem::ParkedMessage("held".into()));
    q.confirm_head_promoted();
    assert!(
        matches!(q[0], InputItem::Message(_)),
        "a parked head becomes a live message"
    );

    let mut command_head = InputQueue::default();
    command_head.push(InputItem::Command("/a".into()));
    command_head.confirm_head_promoted();
    assert!(
        matches!(command_head[0], InputItem::Command(_)),
        "a command head is not promoted"
    );
}

mod sync;
