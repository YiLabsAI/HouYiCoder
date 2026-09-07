//! Tests for the working transcript pane, split out so the render file stays
//! under the size gate. Declared via a path attribute from working_transcript.
use crate::records::TranscriptLine;
use crate::test_support::render_text;
use crate::test_support::working_app;

fn bash_call(command: &str) -> TranscriptLine {
    TranscriptLine::Tool {
        name: "bash".into(),
        tool: "bash".into(),
        status: crate::brief::tool_call_brief("bash", &serde_json::json!({ "command": command })),
        invocation: command.into(),
        outcome: crate::records::ToolOutcome::Success,
        call_id: "c1".into(),
        body: String::new(),
        is_diff: false,
    }
}

fn bash_result() -> TranscriptLine {
    TranscriptLine::Tool {
        name: "result".into(),
        tool: "bash".into(),
        status: String::new(),
        invocation: String::new(),
        outcome: crate::records::ToolOutcome::Success,
        call_id: "c1".into(),
        body: String::new(),
        is_diff: false,
    }
}

#[test]
fn test_hint_single_line() {
    use unicode_width::UnicodeWidthStr;
    let mut app = working_app();
    let command = "ego-browser nodejs <<'EOF'\nconst task = 7\nEOF";
    app.transcript = vec![
        TranscriptLine::User("run it".into()),
        bash_call(command),
        bash_result(),
    ];
    drop(render_text(&app, 32, 12));
    let rows = app.last_all_rows.borrow();
    let hint = rows
        .iter()
        .map(|(_, row)| row)
        .find(|row| row.contains('\u{23bf}'))
        .expect("fold hint");
    assert!(!hint.contains('\n'), "hint must occupy one row: {hint:?}");
    assert!(
        hint.ends_with('\u{2026}'),
        "hidden body needs an ellipsis: {hint}"
    );
    assert!(
        UnicodeWidthStr::width(hint.as_str()) <= 32,
        "hint overflow: {hint}"
    );
}

#[test]
fn test_expand_shows_command() {
    let mut app = working_app();
    let command = "ego-browser nodejs <<'EOF'\nconst task = 7\nEOF";
    app.transcript = vec![
        TranscriptLine::User("run it".into()),
        bash_call(command),
        bash_result(),
    ];
    app.expanded_fold_groups.insert("c1#0".into());
    let out = render_text(&app, 32, 16);
    assert!(
        out.contains("const task = 7"),
        "expanded command body missing: {out}"
    );
    assert!(
        out.contains("EOF)"),
        "expanded command terminator missing: {out}"
    );
}

/// Entering a teammate view must swap the rendered transcript to the child's
/// rows. The slots cache keys on the parent transcript version, and entering
/// the view sets teammate_view without bumping that version, so the cache
/// stayed stale and the parent rows kept rendering after the user drilled in.
/// Regression for the cache-invalidation gap.
#[test]
fn test_teammate_drill_renders_child() {
    let mut app = working_app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "did stuff".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child marker line".into())],
        color: None,
    });
    drop(render_text(&app, 80, 24));
    app.enter_teammate_view_for_sid("c1", false);
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("child marker line"),
        "teammate view must render the child transcript, not the parent: {out}"
    );
}

/// Exiting a teammate view restores the parent transcript. The exit clears
/// teammate_view and bumps the version so the cache rebuilds from the parent
/// — without the bump, the child rows would linger on screen after exit.
#[test]
fn test_exit_teammate_restores_parent() {
    let mut app = working_app();
    app.transcript
        .push(TranscriptLine::User("parent line".into()));
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "did stuff".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child marker line".into())],
        color: None,
    });
    drop(render_text(&app, 80, 24));
    app.enter_teammate_view_for_sid("c1", false);
    drop(render_text(&app, 80, 24));
    app.exit_teammate_view();
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("parent line"),
        "exit restores the parent transcript: {out}"
    );
    assert!(
        !out.contains("child marker line"),
        "child rows must not linger after exit: {out}"
    );
}

/// A fetched child transcript must render once the fetch lands, not stay
/// frozen on the (empty) view held at enter. The fill path bumps the version
/// so the cache rebuilds with the child rows.
#[test]
fn test_teammate_fill_renders_child() {
    use crate::run_control::AgentMessage;
    use crate::transcript::TranscriptFrame;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::ContentChunk;
    use houyicoder_protocol::frontend::session_update::SessionUpdate;
    let mut app = working_app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "did stuff".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    drop(render_text(&app, 80, 24));
    // Enter with an empty fold: view opens empty, fetch fires.
    app.enter_teammate_view_for_sid("c1", true);
    drop(render_text(&app, 80, 24));
    // The fetch lands with the child's assistant text.
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: vec![TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "auth is in src/auth".into(),
            }),
        ))],
    });
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("auth is in src/auth"),
        "fetched child transcript must render after the fill: {out}"
    );
}

/// A steering message typed into a running viewed child pushes an
/// optimistic echo into the view. That push mutates the viewed transcript, so
/// the cache must rebuild or the echo stays invisible until the next
/// turn-boundary refetch lands. Regression for the steering-echo bump site.
#[test]
fn test_steer_echo_renders() {
    use crate::agent_message::FleetEntry;
    use crate::records::TeammateView;
    let mut app = working_app();
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
    app.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });
    drop(render_text(&app, 80, 24));
    app.spawn_run("steer the child".into());
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("steer the child"),
        "the steering echo must render in the teammate view: {out}"
    );
}

#[test]
fn test_cache_version_stable_idle() {
    let mut app = working_app();
    app.transcript.push(TranscriptLine::User("hello".into()));
    let v = app.transcript_version.get().wrapping_add(1);
    app.transcript_version.set(v);
    let out1 = render_text(&app, 80, 24);
    assert!(
        app.display_rows_version.get() != u64::MAX,
        "version should be set after first render"
    );
    let out2 = render_text(&app, 80, 24);
    assert_eq!(out1, out2, "second render should match (cache hit)");
}

/// The spinner is separated from the transcript above it by a blank row. The
/// cached transcript rows and the per-frame live rows are built by two
/// different functions, so the live builder has to be told the transcript is
/// non-empty; when it is not, the blank row silently disappears and the
/// spinner butts up against the last user line.
#[test]
fn test_spinner_keeps_blank_above() {
    let mut app = working_app();
    app.transcript.push(TranscriptLine::User("hello".into()));
    app.agent_busy = true;
    app.run_started = Some(std::time::Instant::now());
    let out = render_text(&app, 80, 24);
    let rows: Vec<&str> = out.lines().collect();
    let spinner = rows
        .iter()
        .position(|r| r.contains("Working…"))
        .expect("the spinner row should render while the agent is busy");
    assert!(
        rows[spinner - 1].trim().is_empty(),
        "a blank row should separate the transcript from the spinner, got:\n{out}"
    );
    assert!(
        rows[spinner - 2].contains("hello"),
        "the user line should sit directly above that blank row, got:\n{out}"
    );
}

/// While a teammate view is open, the parent's live streaming text must not
/// leak into the child's view. build_live_rows suppresses all live rows
/// when teammate_view is Some, so even with live_active + non-empty
/// live_assistant_text the result is zero rows.
#[test]
fn test_view_hides_live_rows() {
    use crate::records::TeammateView;
    let mut app = crate::test_support::working_app();
    app.live_active = true;
    app.live_assistant_text = "parent streaming text".into();
    app.agent_busy = true;
    app.run_started = Some(std::time::Instant::now());
    app.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });
    let live = super::super::live_rows::build_live_rows(
        ratatui::layout::Rect::new(0, 0, 80, 24),
        &app,
        false,
    );
    assert!(
        live.rows.is_empty(),
        "live rows must be empty in teammate view, got {} rows: {:?}",
        live.rows.len(),
        live.rows
    );
    assert!(
        live.all_rows.is_empty(),
        "all_rows must be empty too (stash for selection/copy)"
    );
}
