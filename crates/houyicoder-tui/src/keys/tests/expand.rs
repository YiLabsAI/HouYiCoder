//! The expand chord over the transcript: with no cursor it answers the
//! block the transcript holds latest, and with a cursor the row under it
//! decides. Each block owner is offered the row in turn, and the newest
//! block takes the key when none does.

use super::*;

use crate::records::ToolOutcome;

/// With no cursor, Ctrl+O expands whichever block the transcript holds
/// latest — here the reasoning summary that arrived after the delegation.
/// A single delegation anywhere used to swallow the key in every case.
#[test]
fn test_ctrl_o_expands_latest() {
    let mut app = working_app();
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::Subagent {
            child_sid: "c1".into(),
            subagent_type: "explore".into(),
            summary: "found auth".into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::ThoughtFor {
            ms: Some(19_000),
            reasoning: Some("a long train of reasoning".into()),
            tool_summary: None,
            turn_id: "t1".into(),
        });
    assert!(app.ui.selection.transcript.anchor.is_none(), "no cursor");
    handle_ctrl_o(&mut app);
    assert!(
        app.ui
            .transcript_view
            .expanded
            .thinking
            .contains(&"t1".into()),
        "the later block expands: {:?}",
        app.ui.transcript_view.expanded.thinking
    );
    assert!(
        app.ui.transcript_view.expanded.subagents.is_empty(),
        "the earlier delegation is untouched: {:?}",
        app.ui.transcript_view.expanded.subagents
    );
    handle_ctrl_o(&mut app);
    assert!(
        app.ui.transcript_view.expanded.thinking.is_empty(),
        "the same key collapses what it expanded"
    );
}

/// With no cursor, a completed tool group at the tail answers the chord its
/// summary row advertises. The group rendered with "(ctrl+o to expand)" while
/// the key fell through to a result toggle that changed nothing on screen, so
/// the hint was a promise the key could not keep.
#[test]
fn test_ctrl_o_opens_group() {
    let mut app = working_app();
    app.sessions.current.transcript = vec![
        TranscriptLine::User("hi".into()),
        TranscriptLine::Tool {
            name: "bash".into(),
            tool: "bash".into(),
            status: "printf 'probe-%s' 42".into(),
            invocation: "printf 'probe-%s' 42".into(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            background_task: None,
            body: String::new(),
            is_diff: false,
        },
        TranscriptLine::Tool {
            name: "result".into(),
            tool: "result".into(),
            status: String::new(),
            invocation: String::new(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            background_task: None,
            body: "probe-42".into(),
            is_diff: false,
        },
        TranscriptLine::Agent("done".into()),
    ]
    .into();
    let folded = render_text(&app, 80, 24);
    assert!(
        folded.contains("Ran 1 shell command (ctrl+o to expand)"),
        "the group folds to its summary with the hint: {folded}"
    );
    assert!(
        !folded.contains("probe-42"),
        "the output is folded: {folded}"
    );
    assert!(app.ui.selection.transcript.anchor.is_none(), "no cursor");
    handle_ctrl_o(&mut app);
    let opened = render_text(&app, 80, 24);
    assert!(
        opened.contains("probe-42"),
        "the chord opens the rows the hint points at: {opened}"
    );
}

/// A memory-change notice appended after a completed tool group is the block
/// the transcript holds latest, so the chord opens the notice. Offering the
/// groups first left the notice's own hint unanswered: the key expanded a
/// group the user had already scrolled past.
#[test]
fn test_ctrl_o_opens_notice() {
    let mut app = working_app();
    app.sessions.current.transcript = vec![
        TranscriptLine::User("hi".into()),
        TranscriptLine::Tool {
            name: "bash".into(),
            tool: "bash".into(),
            status: "printf 'probe-%s' 42".into(),
            invocation: "printf 'probe-%s' 42".into(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            background_task: None,
            body: String::new(),
            is_diff: false,
        },
        TranscriptLine::Tool {
            name: "result".into(),
            tool: "result".into(),
            status: String::new(),
            invocation: String::new(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            background_task: None,
            body: "probe-42".into(),
            is_diff: false,
        },
        TranscriptLine::Agent("done".into()),
        TranscriptLine::System(
            "Memory saved by the agent: 1 change · /memory\ncreated deploy-gate\nscope: project"
                .into(),
        ),
    ]
    .into();
    let folded = render_text(&app, 80, 24);
    assert!(
        folded.contains("Ran 1 shell command"),
        "the group folds: {folded}"
    );
    assert!(
        !folded.contains("scope: project"),
        "the notice folds to its summary: {folded}"
    );
    handle_ctrl_o(&mut app);
    let opened = render_text(&app, 80, 24);
    assert!(
        opened.contains("scope: project"),
        "the chord opens the notice it advertises: {opened}"
    );
    assert!(
        !opened.contains("probe-42"),
        "the older group stays folded: {opened}"
    );
}

/// The notice yields to a group the transcript gained afterwards: the same
/// key keeps meaning the latest block, so a later turn's work outranks a
/// notice from an earlier one.
#[test]
fn test_ctrl_o_follows_newest() {
    let mut app = working_app();
    app.sessions.current.transcript = vec![
        TranscriptLine::User("hi".into()),
        TranscriptLine::System(
            "Memory saved by the agent: 1 change · /memory\nscope: project".into(),
        ),
        TranscriptLine::Tool {
            name: "bash".into(),
            tool: "bash".into(),
            status: "printf 'probe-%s' 42".into(),
            invocation: "printf 'probe-%s' 42".into(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            background_task: None,
            body: String::new(),
            is_diff: false,
        },
        TranscriptLine::Tool {
            name: "result".into(),
            tool: "result".into(),
            status: String::new(),
            invocation: String::new(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            background_task: None,
            body: "probe-42".into(),
            is_diff: false,
        },
        TranscriptLine::Agent("done".into()),
    ]
    .into();
    let folded = render_text(&app, 80, 24);
    assert!(
        folded.contains("Memory saved by the agent"),
        "the notice renders as a summary: {folded}"
    );
    handle_ctrl_o(&mut app);
    let opened = render_text(&app, 80, 24);
    assert!(
        opened.contains("probe-42"),
        "the newer group takes the chord: {opened}"
    );
    assert!(
        !opened.contains("scope: project"),
        "the older notice stays folded: {opened}"
    );
}

/// With a cursor, the row under it decides. A reasoning summary the cursor
/// sits on expands even though a delegation follows it in the transcript,
/// which the tail-most rule would otherwise pick.
#[test]
fn test_ctrl_o_follows_cursor() {
    let mut app = working_app();
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::ThoughtFor {
            ms: Some(19_000),
            reasoning: Some("a long train of reasoning".into()),
            tool_summary: None,
            turn_id: "t1".into(),
        });
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::Subagent {
            child_sid: "c1".into(),
            subagent_type: "explore".into(),
            summary: "found auth".into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
    drop(render_text(&app, 80, 24));
    let ri = app
        .ui
        .frame
        .layout
        .last_transcript_rows
        .borrow()
        .iter()
        .position(|(_, text)| text.contains("Thought for"))
        .expect("the reasoning row is on screen");
    let top = app.whole_scroll().top_offset();
    app.ui.selection.transcript.start(0, top + ri);
    handle_ctrl_o(&mut app);
    assert!(
        app.ui
            .transcript_view
            .expanded
            .thinking
            .contains(&"t1".into()),
        "the row under the cursor expands"
    );
    assert!(
        app.ui.transcript_view.expanded.subagents.is_empty(),
        "the later delegation stays collapsed"
    );
}

/// The checklist answers the shared expand chord. It used to sit behind the
/// tail fallback, which claims the chord whenever any foldable block is on
/// screen — in a real session that is always — so the checklist branch never
/// ran and the list could not be opened at all.
#[test]
fn test_ctrl_o_reaches_checklist() {
    let mut app = working_app();
    app.sessions.current.todos.items = vec![
        TodoView {
            content: "write the fix".into(),
            active_form: Some("writing the fix".into()),
            status: TodoStatus::Pending,
        },
        TodoView {
            content: "run the gate".into(),
            active_form: Some("running the gate".into()),
            status: TodoStatus::Pending,
        },
    ];
    // A foldable block at the tail, the state that used to swallow the chord.
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::ThoughtFor {
            ms: Some(9_000),
            reasoning: Some("a train of reasoning".into()),
            tool_summary: None,
            turn_id: "t1".into(),
        });
    assert!(!app.sessions.current.todos.expanded);
    handle_ctrl_o(&mut app);
    assert!(
        app.sessions.current.todos.expanded,
        "the chord reaches the checklist even with a fold at the tail"
    );
    handle_ctrl_o(&mut app);
    assert!(
        !app.sessions.current.todos.expanded,
        "and the same chord closes it again"
    );
}

/// The chord must work with the cursor on the checklist's own summary row.
/// That row publishes a fold key so a click can open the list, and the chord
/// path read the same key and handed it to the transcript, which stored a fold
/// group nothing owned: the key was consumed, the list stayed shut, and the
/// hint printed on the row was a lie.
#[test]
fn test_ctrl_o_opens_todos() {
    let mut app = working_app();
    app.sessions.current.todos.items = (0..6)
        .map(|i| TodoView {
            content: format!("task {i}"),
            active_form: None,
            status: TodoStatus::Pending,
        })
        .collect();
    drop(render_text(&app, 80, 24));
    let summary = app
        .ui
        .frame
        .layout
        .last_transcript_rows
        .borrow()
        .iter()
        .position(|(_, text)| text.contains("hidden:"))
        .expect("the summary row is on screen");
    app.ui.selection.transcript.anchor = Some((0, summary));
    assert!(!app.sessions.current.todos.expanded);
    handle_ctrl_o(&mut app);
    assert!(
        app.sessions.current.todos.expanded,
        "the chord opens the checklist from its own summary row"
    );
}
