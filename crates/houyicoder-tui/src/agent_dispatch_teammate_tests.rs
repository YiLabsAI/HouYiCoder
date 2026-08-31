//! Teammate-view Esc/echo/steer tests split from agent_dispatch_tests
//! to keep the file under the size gate.

use crate::agent_message::AgentMessage;

/// Esc on a viewed running child sends an abort (the view stays); Esc on
/// a completed/non-running child exits the view. The running check reads
/// the fleet entry's completion flag. A child with no fleet entry (running
/// defaults false) exits cleanly.
#[test]
fn test_teammate_esc_abort() {
    use crate::agent_message::FleetEntry;
    use crate::records::TeammateView;
    // Running child: Esc aborts the turn (send_cmd is a no-op without a
    // session, but the decision keeps the view open).
    let mut app = crate::composition::app();
    app.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });
    app.fleet.entries.push(FleetEntry {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 2,
        tokens: 100,
        tool_uses: 0,
        last_activity: None,
        completed: None,
        completed_at: None,
    });
    app.esc_teammate_view_or_abort();
    assert!(
        app.teammate_view.is_some(),
        "a running child keeps the view open for the abort"
    );
    // Completed child: Esc exits the view.
    app.fleet.entries[0].completed = Some("completed".into());
    app.esc_teammate_view_or_abort();
    assert!(
        app.teammate_view.is_none(),
        "a completed child exits the view on Esc"
    );
    // No fleet entry (running defaults false): Esc exits.
    app.teammate_view = Some(TeammateView {
        child_sid: "c2".into(),
        ..Default::default()
    });
    app.esc_teammate_view_or_abort();
    assert!(
        app.teammate_view.is_none(),
        "a child with no fleet entry exits the view on Esc"
    );
}

/// A pending optimistic echo (a steering message sent while viewing) is
/// preserved across a live refetch that lands before the child drains the
/// steering (the next Progress fires at the turn end, before the next
/// turn's drain), so the echo does not vanish mid-turn. Once the child's
/// durable line (a User line with the echo text) appears in the fetched
/// transcript, the echo clears.
#[test]
fn test_teammate_echo_preserved() {
    use crate::records::{TeammateView, TranscriptLine};
    use crate::transcript::TranscriptFrame;
    use houyicoder_protocol::frontend::ContentBlock;
    use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};
    let mut app = crate::composition::app();
    app.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        pending_echo: Some("steer this".into()),
        transcript: vec![TranscriptLine::User("steer this".into())],
        ..Default::default()
    });
    // A refetch whose fetched transcript lacks the echo: preserve it.
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: vec![TranscriptFrame::Session(SessionUpdate::UserMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "child reply".into(),
            }),
        ))],
    });
    let view = app.teammate_view.as_ref().expect("view stays");
    assert!(
        view.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t == "steer this")),
        "the echo is preserved when the fetched transcript lacks it"
    );
    assert_eq!(
        view.pending_echo,
        Some("steer this".into()),
        "echo still pending until the durable line lands"
    );
    // A refetch whose fetched transcript carries the durable steering line:
    // the echo clears (the real line replaced it).
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: vec![TranscriptFrame::Session(SessionUpdate::UserMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "steer this".into(),
            }),
        ))],
    });
    assert!(
        app.teammate_view
            .as_ref()
            .is_some_and(|v| v.pending_echo.is_none()),
        "the echo clears once the durable line lands"
    );
}

/// Steering a completed child surfaces a clear "finished" notice instead
/// of silently dropping on the closed inbox (CC drops silently; this is a
/// UX improvement). The echo line is not appended (the child won't drain
/// it). A running child still steers normally.
#[test]
fn test_steer_completed_surfaces_notice() {
    use crate::agent_message::FleetEntry;
    use crate::records::{TeammateView, TranscriptLine};
    let mut app = crate::composition::app();
    app.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });
    app.fleet.entries.push(FleetEntry {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 3,
        tokens: 100,
        tool_uses: 1,
        last_activity: None,
        completed: Some("completed".into()),
        completed_at: None,
    });
    app.spawn_run("steer this".into());
    // The completed child's inbox is closed, so the steer exits the teammate
    // view + surfaces a notice in the PARENT transcript (visible at the tail)
    // so the user learns the child is done + is back at the parent.
    assert!(
        app.teammate_view.is_none(),
        "steering a completed child exits the view"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("has finished"))),
        "a completed child surfaces a finished notice in the parent transcript"
    );
    // No echo line is appended (the child won't drain it; the view is gone).
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t == "steer this")),
        "no echo appended for a completed child"
    );
    // A running child still steers (echo appended, no notice). The completed
    // steer above exited the view, so re-enter it for the running case.
    app.fleet.entries[0].completed = None;
    app.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });
    app.transcript.clear();
    app.spawn_run("steer running".into());
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("has finished"))),
        "a running child does not surface the finished notice"
    );
    assert!(
        app.teammate_view.as_ref().is_some_and(|v| v
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t == "steer running"))),
        "a running child gets the optimistic echo"
    );
}
