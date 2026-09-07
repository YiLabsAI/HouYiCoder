use super::*;

/// A ChildTranscriptResult reply populates the matching Subagent line's
/// folded_transcript through the same projection as the parent flow
/// (isomorphism: the child renders as TranscriptLines, not an opaque blob).
/// Pins the fill arm + the transcript_from_frames projection so a refactor
/// that drops the in-place swap or the projection fails here.
#[test]
fn test_child_transcript_fills_folded() {
    use crate::records::TranscriptLine;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::ContentChunk;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
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
    match &app.transcript[0] {
        TranscriptLine::Subagent {
            folded_transcript, ..
        } => {
            assert_eq!(
                folded_transcript.len(),
                2,
                "child frames projected to 2 lines"
            );
            assert!(matches!(folded_transcript[0], TranscriptLine::User(_)));
            assert!(matches!(folded_transcript[1], TranscriptLine::Agent(_)));
        }
        other => panic!("subagent line preserved, got {other:?}"),
    }
}

/// An empty frame list (child log missing/unreadable, or the child produced
/// no durable events) surfaces an explicit unavailable line rather than
/// re-showing the placeholder, so a re-expand does not refetch forever.
/// Pins the empty-case guard.
#[test]
fn test_child_transcript_empty_unavailable() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: Vec::new(),
    });
    match &app.transcript[0] {
        TranscriptLine::Subagent {
            folded_transcript, ..
        } => {
            assert_eq!(folded_transcript.len(), 1, "empty -> one unavailable line");
            assert!(
                matches!(&folded_transcript[0], TranscriptLine::System(s) if s.contains("unavailable")),
                "unavailable line surfaces, got {:?}",
                folded_transcript[0]
            );
        }
        other => panic!("subagent line preserved, got {other:?}"),
    }
}

/// The in-place swap preserves the Subagent line's position when trailing
/// lines exist (remove + insert at the same index, not push to tail). Pins
/// position stability so an expanded fold does not yank trailing content.
#[test]
fn test_child_transcript_preserves_position() {
    use crate::records::TranscriptLine;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::ContentChunk;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Agent("before".into()));
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    app.transcript.push(TranscriptLine::Agent("after".into()));
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: vec![TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "child reply".into(),
            }),
        ))],
    });
    assert!(
        matches!(app.transcript[0], TranscriptLine::Agent(_)),
        "before stays at 0"
    );
    assert!(
        matches!(app.transcript[1], TranscriptLine::Subagent { .. }),
        "subagent stays at 1"
    );
    assert!(
        matches!(app.transcript[2], TranscriptLine::Agent(_)),
        "after stays at 2"
    );
}

/// Collapse does not clear an already-loaded folded_transcript, so a
/// re-expand reuses the cached rows without refetching. Pins the retain
/// semantics (the fetch is first-expand-only).
#[test]
fn test_subagent_collapse_keeps_folded() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
        color: None,
    });
    // Expand, then collapse: the child rows survive the collapse.
    app.toggle_tail_expand();
    assert!(app.expanded_subagents.contains("c1"));
    app.toggle_tail_expand();
    assert!(!app.expanded_subagents.contains("c1"), "collapsed");
    match &app.transcript[0] {
        TranscriptLine::Subagent {
            folded_transcript, ..
        } => {
            assert_eq!(folded_transcript.len(), 1, "collapse kept the child rows");
            assert!(matches!(folded_transcript[0], TranscriptLine::Agent(_)));
        }
        other => panic!("subagent line preserved, got {other:?}"),
    }
}

/// Clicking a Subagent delegation's head row expands it inline — the same
/// toggle Ctrl+O drives — instead of starting a drag-select. The head row
/// carries the subagent tag plus a child_sid fold key, so the mouse-down
/// intercept routes the click to the toggle. Pins the click-to-expand path
/// (the head tag + fold key + handle_down branch) so a refactor that reverts
/// the head to a plain selectable row fails here.
#[test]
fn test_click_subagent_head_expands() {
    use crate::records::TranscriptLine;
    use crate::selection::surface::{Surface, TranscriptSurface};
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    let _out = crate::test_support::render_text(&app, 80, 24);
    let rect = app.transcript_rect.get();
    assert!(!app.expanded_subagents.contains("c1"), "starts collapsed");
    {
        let mut surface = TranscriptSurface { app: &mut app };
        surface.handle_down(rect.x, rect.y);
        surface.handle_up();
    }
    assert!(
        app.expanded_subagents.contains("c1"),
        "clicking the subagent head expands the delegation: {:?}",
        app.expanded_subagents
    );
    // Toggle symmetry: a second click on the (still-head) row collapses.
    let _out = crate::test_support::render_text(&app, 80, 24);
    let rect = app.transcript_rect.get();
    {
        let mut surface = TranscriptSurface { app: &mut app };
        surface.handle_down(rect.x, rect.y);
        surface.handle_up();
    }
    assert!(
        !app.expanded_subagents.contains("c1"),
        "clicking an expanded head collapses it"
    );
}

/// toggle_subagent_expand_at_row on a row with no fold key (a non-head row,
/// or an out-of-range index) is a no-op that returns false rather than
/// misfiring on a None fold key. Pins the None branch of the row resolver.
#[test]
fn test_subagent_toggle_no_key() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    // An out-of-range row index has no fold key.
    assert!(
        !app.toggle_subagent_expand_at_row(usize::MAX),
        "out-of-range row returns false, no misfire"
    );
    assert!(
        app.expanded_subagents.is_empty(),
        "nothing expanded on a no-fold-key row"
    );
}

/// A stale row stash (a fold key whose child session id no longer matches any
/// Subagent line in the transcript) defaults to fetch-first rather than
/// treating the line as already loaded. Pins the fallback so a stale stash
/// cannot silently skip a needed fetch.
#[test]
fn test_subagent_toggle_stale_key() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "real".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    // Forge a stale stash: row 0 carries a child id the transcript no longer
    // holds (a post-rebuild drift the resolver must tolerate).
    app.last_row_fold_keys
        .borrow_mut()
        .push(Some("ghost".into()));
    assert!(
        app.toggle_subagent_expand_at_row(0),
        "stale row still toggles (treats the ghost as a delegation)"
    );
    assert!(
        app.expanded_subagents.contains("ghost"),
        "stale fold key expanded under the ghost id"
    );
}

/// Cursor targeting: when the cursor is on a specific Subagent line, Ctrl+O
/// expands that line, not the last one. Pins the cursor walk's spacer logic
/// against the flat content-row space the selection lives in. Without a
/// cursor the walk resolves nothing at all — naming a default is the
/// caller's decision, and the tail-most rule then picks the later line.
#[test]
fn test_subagent_cursor_targeting() {
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
        app.toggle_subagent_expand(),
        "the cursor names a delegation"
    );
    assert!(
        app.expanded_subagents.contains("c1"),
        "cursor on first line expands it"
    );
    assert!(
        !app.expanded_subagents.contains("c2"),
        "the second line is not expanded when the cursor targets the first"
    );
    app.expanded_subagents.clear();
    app.selection.anchor = None;
    assert!(
        !app.toggle_subagent_expand(),
        "with no cursor the walk resolves nothing"
    );
    assert!(app.toggle_tail_expand(), "the tail-most rule picks one");
    assert!(
        app.expanded_subagents.contains("c2"),
        "and it is the later line"
    );
}

/// A fetched wire child frame converts to the live-frame shape the
/// transcript projection consumes, so child rows render through the same
/// pipeline as the parent flow. Pins the From impl at the driver boundary.
#[test]
fn test_child_frame_converts() {
    use houyicoder_protocol::envelope::ChildTranscriptFrame;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};
    let wire = ChildTranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Text {
            text: "child reply".into(),
        },
    )));
    let frame: TranscriptFrame = wire.into();
    assert!(
        matches!(
            frame,
            TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(_))
        ),
        "session frame converts: {frame:?}"
    );
    let wire_acpx = ChildTranscriptFrame::Acpx(houyicoder_protocol::acpx::AcpxNotification::new(
        houyicoder_protocol::acpx::AcpxMethod::ToolProgress,
        serde_json::Value::Null,
    ));
    let acpx: TranscriptFrame = wire_acpx.into();
    assert!(
        matches!(acpx, TranscriptFrame::Acpx(_)),
        "acpx frame converts: {acpx:?}"
    );
}

/// A parent transcript rebuild must not wipe the fetched child rows from a
/// Subagent line. The line is frame-derived, so a rebuild re-projects it
/// empty; the merge carries the old folded_transcript over when the
/// child_sid matches. Pins the retain semantics the rebuild otherwise
/// violates.
#[test]
fn test_subagent_folded_survives_rebuild() {
    use crate::records::TranscriptLine;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::ContentChunk;
    let call = TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new("ag1", "agent")
            .status(ToolCallStatus::Completed)
            .raw_input(serde_json::json!({"subagent_type": "explore"})),
    ));
    let result = TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "ag1",
        ToolCallUpdateFields::new()
            .raw_output(serde_json::json!({"agentId": "c1", "content": "found auth"})),
    )));
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::Frame(call));
    app.handle_agent_message(AgentMessage::Frame(result));
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Subagent { .. })),
        "subagent line projected from the agent-tool result"
    );
    // Fill folded_transcript (the on-expand fetch landing).
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: vec![TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "child reply".into(),
            }),
        ))],
    });
    // A parent rebuild re-projects frames; the fetched child rows survive.
    app.rebuild_transcript();
    match app
        .transcript
        .iter()
        .find(|l| matches!(l, TranscriptLine::Subagent { .. }))
    {
        Some(TranscriptLine::Subagent {
            child_sid,
            folded_transcript,
            ..
        }) => {
            assert_eq!(child_sid, "c1");
            assert_eq!(
                folded_transcript.len(),
                1,
                "fetched child rows survived the rebuild"
            );
            assert!(matches!(folded_transcript[0], TranscriptLine::Agent(_)));
        }
        other => panic!("subagent line survived, got {other:?}"),
    }
}
