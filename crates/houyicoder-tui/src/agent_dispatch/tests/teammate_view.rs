use super::*;

/// Enter on a Subagent line opens the teammate view targeting the line at
/// the cursor, not the last one. The view carries the subagent_type +
/// summary for the banner. Pins the cursor-targeting entry so a refactor
/// that opens the wrong child fails here.
#[test]
fn test_enter_teammate_targets_cursor() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "first".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
        color: None,
    });
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c2".into(),
        subagent_type: "plan".into(),
        summary: "second".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child reply 2".into())],
        color: None,
    });
    app.selection.start(0, 0);
    assert!(
        app.enter_teammate_view(),
        "cursor on a Subagent line enters"
    );
    let view = app.teammate_view.as_ref().expect("teammate view is open");
    assert_eq!(view.child_sid, "c1", "cursor targets the first line");
    assert_eq!(view.subagent_type, "explore");
    assert_eq!(view.prompt, "");
    // A pre-loaded fold copies into the view so it renders immediately.
    assert_eq!(view.transcript.len(), 1);
    assert!(matches!(view.transcript[0], TranscriptLine::Agent(_)));
    // active_transcript swaps to the child's, not the parent's.
    assert_eq!(
        app.active_transcript().len(),
        1,
        "active transcript is the child's"
    );
}

/// Esc clears the teammate view, returning active_transcript to the parent.
#[test]
fn test_exit_teammate_clears_view() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Agent("parent".into()));
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "first".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
        color: None,
    });
    assert!(app.enter_teammate_view());
    assert!(app.teammate_view.is_some());
    assert_eq!(app.active_transcript().len(), 1, "viewing child");
    app.exit_teammate_view();
    assert!(app.teammate_view.is_none(), "view cleared on exit");
    assert_eq!(
        app.active_transcript().len(),
        2,
        "parent transcript restored"
    );
}

/// A fetched child frame fills the teammate view's transcript through the
/// same projection the inline fold receives, so the drilled-in view is
/// isomorphic with the expanded fold, not a parallel simplification. Pins
/// the T36e isomorphism contract: child view renders via the main pipeline.
#[test]
fn test_teammate_view_fill_isomorphic() {
    use crate::records::TranscriptLine;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::ContentChunk;
    use houyicoder_protocol::frontend::session_update::SessionUpdate;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    // Enter with an unloaded fold: the view opens empty and the fetch fires.
    assert!(app.enter_teammate_view());
    assert_eq!(
        app.teammate_view.as_ref().unwrap().transcript.len(),
        0,
        "view empty until fetch lands"
    );
    let frames = vec![
        TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
            ContentBlock::Text {
                text: "find auth".into(),
            },
        ))),
        TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text {
                text: "auth is in src/auth".into(),
            },
        ))),
    ];
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames,
    });
    let view = app.teammate_view.as_ref().unwrap();
    assert_eq!(view.transcript.len(), 2, "view filled by the fetch");
    assert!(matches!(view.transcript[0], TranscriptLine::User(_)));
    assert!(matches!(view.transcript[1], TranscriptLine::Agent(_)));
    // Isomorphism: the fold-group and the view carry the same projected rows.
    let folded = match &app.transcript[0] {
        TranscriptLine::Subagent {
            folded_transcript, ..
        } => folded_transcript.clone(),
        other => panic!("subagent line preserved, got {other:?}"),
    };
    assert_eq!(
        view.transcript.len(),
        folded.len(),
        "view and fold hold the same row count"
    );
    assert_eq!(
        view.transcript
            .iter()
            .map(|l| l.render())
            .collect::<Vec<_>>(),
        folded.iter().map(|l| l.render()).collect::<Vec<_>>(),
        "view and fold render identically"
    );
}

/// A fetch landing for a child the user is NOT viewing must not clobber the
/// view's transcript. Pins the child_sid match guard.
#[test]
fn test_other_child_keeps_view() {
    use crate::records::TranscriptLine;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::ContentChunk;
    use houyicoder_protocol::frontend::session_update::SessionUpdate;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "first".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("viewed child".into())],
        color: None,
    });
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c2".into(),
        subagent_type: "plan".into(),
        summary: "second".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    app.selection.start(0, 0);
    assert!(app.enter_teammate_view());
    assert_eq!(app.teammate_view.as_ref().unwrap().child_sid, "c1");
    // A fetch for c2 arrives while viewing c1.
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c2".into(),
        frames: vec![TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "other child".into(),
            }),
        ))],
    });
    let view = app.teammate_view.as_ref().unwrap();
    assert_eq!(view.child_sid, "c1", "view unchanged");
    assert_eq!(view.transcript.len(), 1, "view transcript not clobbered");
    assert!(
        matches!(view.transcript[0], TranscriptLine::Agent(ref s) if s == "viewed child"),
        "the viewed child's rows are intact"
    );
}

/// Enter on a transcript with no Subagent line returns false and opens no
/// view, so the caller falls through to submit. Pins the no-target guard.
#[test]
fn test_enter_teammate_no_target() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Agent("plain".into()));
    assert!(!app.enter_teammate_view(), "no Subagent line to target");
    assert!(app.teammate_view.is_none());
}

/// A cursor whose content row falls past the last transcript line misses
/// every line in the walk and falls back to the most recent Subagent.
/// Pins the walk's terminal None + the fallback so a cursor below the
/// tail still drills into the latest delegation.
#[test]
fn test_cursor_past_tail_fallback() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "first".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("reply".into())],
        color: None,
    });
    // A content row well past the one line in the transcript.
    app.selection.start(999, 999);
    assert!(
        app.enter_teammate_view(),
        "cursor past tail falls back to the last Subagent"
    );
    assert_eq!(app.teammate_view.as_ref().unwrap().child_sid, "c1");
}

/// A collapsed fold-group before a Subagent shifts the Subagent's content
/// row in the fold-aware space the mouse sets. The cursor walk mirrors the
/// render path (display_slots), so a click on the first of two delegations
/// after a folded tool group targets that delegation rather than the
/// fallback last.
#[test]
fn test_cursor_after_fold_group() {
    use crate::records::{ToolOutcome, TranscriptLine};
    let mut app = crate::composition::app();
    // Two tool calls form a collapsed Summary slot before the Subagents.
    app.transcript.push(TranscriptLine::Tool {
        name: "bash".into(),
        tool: "bash".into(),
        status: "ls".into(),
        invocation: "ls".into(),
        outcome: ToolOutcome::Success,
        call_id: "t1".into(),
        body: String::new(),
        is_diff: false,
    });
    app.transcript.push(TranscriptLine::Tool {
        name: "result".into(),
        tool: "bash".into(),
        status: String::new(),
        invocation: String::new(),
        outcome: ToolOutcome::Success,
        call_id: "t1".into(),
        body: "ok\nline2\nline3".into(),
        is_diff: false,
    });
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "first".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("reply1".into())],
        color: None,
    });
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c2".into(),
        subagent_type: "plan".into(),
        summary: "second".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("reply2".into())],
        color: None,
    });
    // c1 sits at transcript index 2. fold_aware_rows gives its start row in
    // the same space the mouse sets content_row. A flat walk would land it
    // one slot earlier inside the Summary and miss, falling back to c2.
    let c1_row = app.fold_aware_rows(Some(2));
    app.selection.start(0, c1_row);
    assert!(app.enter_teammate_view(), "cursor on c1 enters");
    assert_eq!(
        app.teammate_view.as_ref().unwrap().child_sid,
        "c1",
        "cursor after a folded group targets the clicked delegation, not the fallback"
    );
    // The flat walk would have resolved to c2 (last) — confirm it does not.
    assert!(
        !app.expanded_subagents.contains("c2"),
        "the non-targeted delegation is untouched"
    );
}

/// An AgentStatus message inserts a fleet entry; a second message for the
/// same agent updates it in place rather than appending.
#[test]
fn test_agent_status_updates_fleet() {
    let mut app = crate::composition::app();
    app.handle_agent_message(crate::run_control::AgentMessage::AgentStatus {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 1,
        tokens: 100,
        tool_uses: 2,
        last_activity: Some("grep".into()),
        completed: None,
    });
    assert_eq!(app.fleet.entries.len(), 1);
    assert_eq!(app.fleet.entries[0].agent_id, "c1");
    assert_eq!(app.fleet.entries[0].turn, 1);
    assert!(app.fleet.entries[0].completed.is_none());

    app.handle_agent_message(crate::run_control::AgentMessage::AgentStatus {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 3,
        tokens: 300,
        tool_uses: 5,
        last_activity: Some("edit".into()),
        completed: Some("completed".into()),
    });
    assert_eq!(app.fleet.entries.len(), 1, "same agent updates in place");
    assert_eq!(app.fleet.entries[0].turn, 3);
    assert_eq!(app.fleet.entries[0].completed.as_deref(), Some("completed"));
}

/// A status for the child in the teammate view auto-exits the view on an
/// abnormal terminal (killed/failed/deadline); a normal completion leaves
/// the view open so the user can read the full transcript. A status for a
/// different child never touches the view.
#[test]
fn test_teammate_view_auto_exit() {
    use crate::records::TeammateView;
    use crate::run_control::AgentMessage;
    let mut app = crate::composition::app();
    app.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });
    // A normal completion: the view stays.
    app.handle_agent_message(AgentMessage::AgentStatus {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 2,
        tokens: 200,
        tool_uses: 1,
        last_activity: None,
        completed: Some("completed".into()),
    });
    assert!(app.teammate_view.is_some(), "completed stays for review");
    // A failure: the view auto-exits back to the parent.
    app.handle_agent_message(AgentMessage::AgentStatus {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 2,
        tokens: 200,
        tool_uses: 1,
        last_activity: None,
        completed: Some("failed".into()),
    });
    assert!(
        app.teammate_view.is_none(),
        "abnormal terminal auto-exits the teammate view"
    );
    // A status for a different child does not touch the view.
    app.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });
    app.handle_agent_message(AgentMessage::AgentStatus {
        agent_id: "c2".into(),
        subagent_type: "plan".into(),
        turn: 1,
        tokens: 10,
        tool_uses: 0,
        last_activity: None,
        completed: Some("failed".into()),
    });
    assert!(
        app.teammate_view.is_some(),
        "a different child status does not exit the view"
    );
    // A turn-limit (max_turns hit): partial output, the view stays for review.
    app.handle_agent_message(AgentMessage::AgentStatus {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 9,
        tokens: 900,
        tool_uses: 4,
        last_activity: None,
        completed: Some("turn_limit".into()),
    });
    assert!(
        app.teammate_view.is_some(),
        "turn-limit leaves the view for partial-output review"
    );
}

/// A running status (no completed) for the viewed child stamps the
/// last-fetched turn + keeps the view open: the live refetch debounce
/// fires once per turn. A same-turn echo does not refire. A different
/// child does not touch the viewed child's turn.
#[test]
fn test_teammate_live_refetch() {
    use crate::records::TeammateView;
    use crate::run_control::AgentMessage;
    let mut app = crate::composition::app();
    app.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });
    app.handle_agent_message(AgentMessage::AgentStatus {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 2,
        tokens: 200,
        tool_uses: 1,
        last_activity: None,
        completed: None,
    });
    assert_eq!(
        app.teammate_view.as_ref().and_then(|v| v.last_fetched_turn),
        Some(2),
        "running status stamps the viewed child's last-fetched turn"
    );
    assert!(app.teammate_view.is_some(), "running keeps the view open");
    app.handle_agent_message(AgentMessage::AgentStatus {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 2,
        tokens: 210,
        tool_uses: 1,
        last_activity: None,
        completed: None,
    });
    assert_eq!(
        app.teammate_view.as_ref().and_then(|v| v.last_fetched_turn),
        Some(2),
        "a same-turn echo does not refire the refetch guard"
    );
    app.handle_agent_message(AgentMessage::AgentStatus {
        agent_id: "c2".into(),
        subagent_type: "plan".into(),
        turn: 5,
        tokens: 10,
        tool_uses: 0,
        last_activity: None,
        completed: None,
    });
    assert_eq!(
        app.teammate_view.as_ref().and_then(|v| v.last_fetched_turn),
        Some(2),
        "a different child status does not touch the viewed child's turn"
    );
}
