//! Teammate-view Esc/echo/steer tests split from agent_dispatch_tests
//! to keep the file under the size gate.

use crate::agent_message::AgentMessage;

/// Esc on a viewed child only interrupts its current turn; it never exits
/// the view. A running child gets a per-turn cancel and the view stays; an
/// idle child is a no-op on the run that pops a toast reminding the exit
/// gesture (shift+↑↓). Exit is on Shift+Up/Down, which ignores the running
/// state — tested at the key layer.
#[test]
fn test_abort_viewed_child_turn() {
    use crate::agent_message::FleetEntry;
    use crate::records::TeammateView;
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
        started_at: None,
    });
    app.abort_viewed_child_turn();
    assert!(
        app.teammate_view.is_some(),
        "Esc on a running child aborts the turn but keeps the view open"
    );
    assert!(
        app.notifications.current().is_none(),
        "a running child sends a cancel, not the exit-hint toast"
    );
    app.fleet.entries[0].completed = Some("completed".into());
    app.abort_viewed_child_turn();
    assert!(
        app.teammate_view.is_some(),
        "Esc on an idle child keeps the view — exit is on Shift+Up/Down"
    );
    assert!(
        app.notifications
            .current()
            .is_some_and(|n| n.key == "teammate-exit-hint"),
        "an idle Esc pops the exit-gesture toast instead of staying silent"
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
/// of silently dropping on the closed inbox. The echo line is not
/// appended (the child won't drain it). A running child still steers
/// normally.
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
        started_at: None,
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

// Child-transcript attribution baseline. These characterize how fetched
// frames land on a Subagent row before any fix, so a later change can be
// judged against current behavior. No product code changes here.

/// Two Subagent rows share a child_sid: the result handler uses rposition and
/// swaps into the LAST matching row, leaving the first untouched. Isolates the
/// rposition mechanism. The duplicate is synthetic — child_sid is the child
/// session id, unique per spawn, so normal flow cannot produce it.
#[test]
fn test_child_transcript_last_row() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    for _ in 0..2 {
        app.push_transcript_line(TranscriptLine::Subagent {
            child_sid: "c1".into(),
            subagent_type: "explore".into(),
            summary: "old summary".into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
    }
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: Vec::new(),
    });
    let folded: Vec<&[TranscriptLine]> = app
        .transcript
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::Subagent {
                folded_transcript, ..
            } => Some(folded_transcript.as_slice()),
            _ => None,
        })
        .collect();
    assert_eq!(folded.len(), 2);
    assert!(
        folded[0].is_empty(),
        "first row untouched — rposition skipped it"
    );
    assert!(!folded[1].is_empty(), "last row received the swap");
}

/// A running child with no Subagent row (the row is created by the result
/// frame, which has not landed): rposition finds no match, so the fetched
/// frames find no transcript anchor. This is the reachable transcript-level
/// failure for a running child whose result frame has not landed — not the
/// duplicate-row case. (A teammate view that is open would still receive the
/// frames via fill_teammate_view; this test isolates the transcript path with
/// no view set.)
#[test]
fn test_child_transcript_no_row() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.fleet.entries.push(crate::agent_message::FleetEntry {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 1,
        tokens: 50,
        tool_uses: 0,
        last_activity: None,
        completed: None,
        completed_at: None,
        started_at: None,
    });
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: Vec::new(),
    });
    assert!(
        app.transcript
            .iter()
            .all(|l| !matches!(l, TranscriptLine::Subagent { child_sid, .. } if child_sid == "c1")),
        "no row exists — frames found no anchor and were dropped"
    );
}

/// Reachability pin: each agent-tool result frame carries a fresh child session
/// id, so two normal delegations produce two different sids. A result for one
/// child never lands on the other's row. The duplicate-row rposition concern
/// is unreachable in production; the no-row drop, not duplicate rows, is the
/// failure to address.
#[test]
fn test_child_sid_unique() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.push_transcript_line(TranscriptLine::Subagent {
        child_sid: "child-A".into(),
        subagent_type: "explore".into(),
        summary: "first".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    app.push_transcript_line(TranscriptLine::Subagent {
        child_sid: "child-B".into(),
        subagent_type: "explore".into(),
        summary: "second".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "child-A".into(),
        frames: Vec::new(),
    });
    let child_b = app
        .transcript
        .iter()
        .find_map(|l| match l {
            TranscriptLine::Subagent {
                child_sid,
                folded_transcript,
                ..
            } if child_sid == "child-B" => Some(folded_transcript.clone()),
            _ => None,
        })
        .unwrap_or_default();
    assert!(
        child_b.is_empty(),
        "child-B untouched — sids differ, no cross-talk"
    );
}

/// A running child has no fold-group row yet (the row is created when the
/// result lands). Entering its view must still name the agent type in the
/// banner, falling back to the live agent list entry which knows the type
/// from spawn. prompt stays empty (the live entry does not carry it); color
/// stays None (only the result sets it).
#[test]
fn test_view_fleet_type_fallback() {
    use crate::agent_message::FleetEntry;
    let mut app = crate::composition::app();
    app.fleet.entries.push(FleetEntry {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 1,
        tokens: 50,
        tool_uses: 0,
        last_activity: None,
        completed: None,
        completed_at: None,
        started_at: None,
    });
    app.enter_teammate_view_for_sid("c1", true);
    let view = app
        .teammate_view
        .as_ref()
        .expect("view opens for a running child even with no fold-group row");
    assert_eq!(
        view.subagent_type, "explore",
        "banner takes the agent type from the live entry when no fold row exists"
    );
    assert!(
        view.prompt.is_empty(),
        "prompt stays empty — the live entry does not carry it"
    );
    assert!(
        view.color.is_none(),
        "color stays None — only the result frame sets it"
    );
}

/// When a fold-group row already exists (completed child), the fleet fallback
/// must not overwrite its subagent_type. The guard checks is_empty first.
#[test]
fn test_view_fold_row_wins() {
    use crate::agent_message::FleetEntry;
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.push_transcript_line(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "plan".into(),
        summary: "done".into(),
        prompt: "do the thing".into(),
        folded_transcript: Vec::new(),
        color: Some("blue".into()),
    });
    app.fleet.entries.push(FleetEntry {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 2,
        tokens: 100,
        tool_uses: 0,
        last_activity: None,
        completed: Some("completed".into()),
        completed_at: None,
        started_at: None,
    });
    app.enter_teammate_view_for_sid("c1", true);
    let view = app.teammate_view.as_ref().expect("view opens");
    assert_eq!(
        view.subagent_type, "plan",
        "fold-row value wins — fleet fallback did not overwrite"
    );
    assert_eq!(view.prompt, "do the thing", "fold-row prompt preserved");
    assert_eq!(
        view.color.as_deref(),
        Some("blue"),
        "fold-row color preserved"
    );
}

/// A running child whose sidechain log is still empty (just spawned, first
/// turn not landed) must surface a "starting" hint, not "unavailable" — the
/// latter implies a fetch failure. The empty-frames branch checks the live
/// agent list to tell the two apart.
#[test]
fn test_running_child_starting() {
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
        turn: 0,
        tokens: 0,
        tool_uses: 0,
        last_activity: None,
        completed: None,
        completed_at: None,
        started_at: None,
    });
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: Vec::new(),
    });
    let view = app.teammate_view.as_ref().expect("view stays");
    assert!(
        view.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("starting"))),
        "a running child with no log yet shows a starting hint, not unavailable"
    );
    assert!(
        !view
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("unavailable"))),
        "a running child must not read as a fetch failure"
    );
}

/// A completed child whose fetch returns empty frames is a real failure (the
/// log should exist), so it stays "unavailable" — not relabeled "starting".
#[test]
fn test_completed_child_unavailable() {
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
        started_at: None,
    });
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: Vec::new(),
    });
    let view = app.teammate_view.as_ref().expect("view stays");
    assert!(
        view.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("unavailable"))),
        "a completed child with empty frames reads as a real fetch failure"
    );
}
