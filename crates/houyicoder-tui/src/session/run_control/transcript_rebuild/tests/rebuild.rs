use super::*;

/// A ToolCall frame renders immediately without waiting for another boundary.
#[test]
fn test_frame_renders_immediately() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "glob"));
    let has_call = app.transcript().iter().any(|l| {
        matches!(
            l,
            TranscriptLine::Tool {
                tool,
                name,
                ..
            } if *tool == "glob" && name != "result"
        )
    });
    assert!(
        has_call,
        "ToolCall renders without an ask: {:?}",
        app.transcript()
    );
}

/// A user boundary between a tool call and its result must not split the pair.
#[test]
fn test_pair_keeps_order() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "glob"));
    pump(&mut app, user_msg("(interrupted)"));
    pump(&mut app, tool_result("c1"));
    let call_at = app.transcript()
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if name != "result" && call_id == "c1"))
        .expect("call row present");
    let result_at = app.transcript()
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if name == "result" && call_id == "c1"))
        .expect("result row present");
    assert_eq!(
        result_at,
        call_at + 1,
        "result is adjacent to its call (paired, not orphaned): {:?}",
        app.transcript()
    );
}

/// A system receipt raised before a run keeps its position above that run's
/// rows throughout a rebuild: a row the frontend raised must not float above
/// or below the frames it was raised among.
#[test]
fn test_system_row_above_run() {
    let mut app = fresh_app();
    app.system_line("debug: logging to /tmp/houyi.log");
    pump(&mut app, user_msg("analyze this dir"));
    pump(&mut app, agent_msg("ok"));
    let sys_at = app
        .transcript()
        .iter()
        .position(|l| matches!(l, TranscriptLine::System(_)))
        .expect("system line present");
    let user_at = app
        .transcript()
        .iter()
        .position(|l| matches!(l, TranscriptLine::User(_)))
        .expect("user line present");
    assert!(
        sys_at < user_at,
        "system line precedes the run's rows: {:?}",
        app.transcript()
    );
}

/// A slash-command echo survives a rebuild exactly once: its own frame holds
/// the one copy, so a run's frames cannot add a second.
#[test]
fn test_slash_echo_survives_rebuild() {
    let mut app = fresh_app();
    app.push_unanswered_echo("/model".into());
    pump(&mut app, user_msg("analyze this dir"));
    pump(&mut app, agent_msg("ok"));
    let echoes = app
        .transcript()
        .iter()
        .filter(|l| matches!(l, TranscriptLine::User(t) if t == "/model"))
        .count();
    assert_eq!(
        echoes,
        1,
        "the command echo keeps one copy through rebuild: {:?}",
        app.transcript()
    );
}

/// A submission in an app with no runner connected is the frontend's own row:
/// nothing carries it to a model, so no reply frame will follow. The log holds
/// it, so a full rebuild re-derives the echo instead of dropping it, and finds
/// no reply row beside it.
#[test]
fn test_submission_echo_one_row() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.input.editor.set("hi".to_string());
    app.submit_input();
    app.rebuild_after_frame_edit();
    let echoes = app
        .transcript()
        .iter()
        .filter(|l| matches!(l, TranscriptLine::User(t) if t == "hi"))
        .count();
    assert_eq!(
        echoes,
        1,
        "the submission echo survives a full rebuild: {:?}",
        app.transcript()
    );
    assert!(
        !app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::Agent(_) | TranscriptLine::Read { .. })),
        "no reply frame, so no reply row: {:?}",
        app.transcript()
    );
}

/// A context view refreshed while its own frame sits below the turn boundary
/// still reaches the transcript. The reused rows belong to the frames above
/// that boundary, so a grid whose frame was replaced in place has to be
/// derived again rather than kept as the row it used to be.
#[test]
fn test_context_refresh_after_turn() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.push_transcript_line(TranscriptLine::ContextGrid(composition::context_view()));
    app.transcript_mut().push_frame(user_msg("go"));
    app.system_line("marker");
    let mut fresh = composition::context_view();
    fresh.suggestions.push(ContextSuggestion {
        severity: SuggestionSeverity::Info,
        title: "refreshed".into(),
        detail: "the reply that arrived after the turn boundary".into(),
        savings_tokens: None,
    });
    app.replace_context_view(fresh);
    assert!(
        app.transcript().iter().any(|l| matches!(
            l,
            TranscriptLine::ContextGrid(v)
                if v.suggestions.iter().any(|s| s.title == "refreshed")
        )),
        "the refreshed grid is the row the log derives: {:?}",
        app.transcript().len()
    );
}

/// A slash-prefixed input that matches no command is a message to the model,
/// so the server carries its own frame for the same text: the local echo and
/// that frame must render one row, not two.
#[test]
fn test_slash_message_single_row() {
    let mut app = fresh_app();
    app.push_transcript_line(TranscriptLine::User("/nope".into()));
    app.drop_tentative_echo();
    app.push_transcript_line(TranscriptLine::User("/nope".into()));
    pump(&mut app, user_msg("/nope"));
    let rows = app
        .transcript()
        .iter()
        .filter(|l| matches!(l, TranscriptLine::User(t) if t == "/nope"))
        .count();
    assert_eq!(
        rows,
        1,
        "the submitted message keeps one row: {:?}",
        app.transcript()
    );
}

#[test]
fn test_rewind_rebuilds_tail() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "glob"));
    pump(&mut app, user_msg("second"));
    assert!(
        app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("second"))),
        "second user echo present before rewind"
    );
    app.rewind_to_last_user_input();
    let stale = app
        .transcript()
        .iter()
        .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("second")));
    assert!(
        !stale,
        "rewind dropped the second echo (no stale line): {:?}",
        app.transcript()
    );
    let kept = app
        .transcript()
        .iter()
        .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("go")));
    assert!(
        kept,
        "first user echo survives rewind: {:?}",
        app.transcript()
    );
}

/// Rebuilding a frame batch must replace the changing tail without duplicates.
#[test]
fn test_batch_keeps_single() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    // Push two frames directly without an intervening rebuild (a batch), then
    // rebuild once — the incremental path must handle the non-empty suffix.
    app.transcript_mut().push_frame(tool_call("c1", "glob"));
    app.transcript_mut().push_frame(tool_result("c1"));
    app.rebuild_transcript();
    let calls = app.transcript()
        .iter()
        .filter(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if *name != "result" && call_id == "c1"))
        .count();
    let results = app.transcript()
        .iter()
        .filter(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if *name == "result" && call_id == "c1"))
        .count();
    assert_eq!(
        calls,
        1,
        "no duplicate call row after batch+rebuild: {:?}",
        app.transcript()
    );
    assert_eq!(
        results,
        1,
        "no duplicate result row after batch+rebuild: {:?}",
        app.transcript()
    );
}

/// todo_write renders through the checklist without adding a tool chip.
#[test]
fn test_todo_skips_chip() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, todo_write_frame("c1", &[("task one", "pending")]));
    let has_tool_row = app
        .transcript()
        .iter()
        .any(|l| matches!(l, TranscriptLine::Tool { tool, .. } if *tool == "todo_write"));
    assert!(
        !has_tool_row,
        "todo_write must not render a transcript tool row (the widget owns it): {:?}",
        app.transcript()
    );
}

/// A later user boundary is not a checklist update. Mid-turn injection and a
/// new message must retain the last todo-write state until another write or an
/// explicit session clear replaces it.
#[test]
fn test_todo_survives_user() {
    let mut app = fresh_app();
    app.start_run_for_test(0);
    pump(&mut app, user_msg("go"));
    pump(
        &mut app,
        todo_write_frame("c1", &[("task one", "in_progress")]),
    );
    assert_eq!(app.sessions.current.todos.items.len(), 1);

    pump(&mut app, user_msg("mid-turn note"));

    assert_eq!(app.sessions.current.todos.items.len(), 1);
    assert_eq!(app.sessions.current.todos.items[0].content, "task one");
}

#[test]
fn test_resumed_todo_paused() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("old run"));
    pump(
        &mut app,
        todo_write_frame("c1", &[("unfinished", "in_progress")]),
    );

    assert_eq!(
        app.sessions.current.todos.items[0].status,
        TodoStatus::Paused
    );
}

#[test]
fn test_resumed_batches_paused() {
    let mut app = fresh_app();
    pump(
        &mut app,
        todo_write_frame("c1", &[("unfinished", "pending")]),
    );
    pump(
        &mut app,
        todo_write_frame("c2", &[("unfinished", "in_progress")]),
    );

    assert_eq!(
        app.sessions.current.todos.items[0].status,
        TodoStatus::Paused
    );
}

/// A rewind replays the truncated transcript as restored history: the
/// all-completed list clears with its stamps instead of re-entering the
/// completion visibility window.
#[test]
fn test_rewind_clears_completed() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(
        &mut app,
        todo_write_frame("c1", &[("task one", "in_progress")]),
    );
    pump(
        &mut app,
        todo_write_frame("c2", &[("task one", "completed")]),
    );
    assert!(
        app.sessions
            .current
            .todos
            .completion_at
            .contains_key("task one")
    );
    pump(&mut app, user_msg("later"));

    app.rewind_to_last_user_input();

    assert!(app.sessions.current.todos.items.is_empty());
    assert!(app.sessions.current.todos.completion_at.is_empty());
}

/// The other side of a rewind: open work survives as restored history, and
/// the stamps recorded before the rewind are cleared instead of riding into
/// the replay.
#[test]
fn test_rewind_keeps_open() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(
        &mut app,
        todo_write_frame(
            "c1",
            &[("task one", "in_progress"), ("task two", "pending")],
        ),
    );
    pump(
        &mut app,
        todo_write_frame("c2", &[("task one", "completed"), ("task two", "pending")]),
    );
    assert!(
        app.sessions
            .current
            .todos
            .completion_at
            .contains_key("task one")
    );
    pump(&mut app, user_msg("later"));

    app.rewind_to_last_user_input();

    assert_eq!(app.sessions.current.todos.items.len(), 2);
    assert!(app.sessions.current.todos.completion_at.is_empty());
}

/// A tool call with no result does not move the current-turn boundary backward.
#[test]
fn test_hanging_call_keeps_prefix() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "glob"));
    pump(&mut app, user_msg("second"));
    assert_eq!(app.current_turn_start(), 3);
}

fn run_record(ms: u64) -> TranscriptFrame {
    TranscriptFrame::Acpx(AcpxNotification::new(
        AcpxMethod::ContextRunCompleted,
        json!({ "ms": ms }),
    ))
}

/// The mark that says the message beside it was delivered into the turn already
/// running. The host writes it right after the message, so a rebuild can run
/// between the two.
fn delivery_mark() -> TranscriptFrame {
    TranscriptFrame::Acpx(AcpxNotification::new(
        AcpxMethod::ContextMidTurnInput,
        json!({}),
    ))
}

/// The rows a turn's summary row appears as, oldest first, with the duration
/// each one carries.
fn summary_rows(app: &App) -> Vec<Option<u64>> {
    app.transcript()
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::ThoughtFor { ms, .. } => Some(*ms),
            _ => None,
        })
        .collect()
}

/// A message queued into a running turn reads as a turn of its own until the
/// mark that says it was delivered arrives, and a block derived in that first
/// read writes the summary row of the turn the message was delivered into. The
/// mark lands after the block was derived, so the block has to be derived again
/// rather than kept: with its rows kept, the premature row stays for the rest
/// of the turn and a second one joins it when the record closes the turn.
#[test]
fn test_mark_rederives_prefix_block() {
    let mut app = fresh_app();
    app.start_run_for_test(0);
    pump(&mut app, user_msg("go"));
    pump(&mut app, thought_msg("thinking"));
    pump(&mut app, user_msg("mid-turn note"));
    app.rebuild_transcript();
    assert_eq!(
        summary_rows(&app).len(),
        1,
        "the message reads as a turn of its own until its mark arrives: {:?}",
        app.transcript()
    );

    pump(&mut app, delivery_mark());
    pump(&mut app, run_record(2_000));
    app.rebuild_transcript();

    assert_eq!(
        summary_rows(&app),
        vec![Some(2_000)],
        "one row for the one turn, closed by its record: {:?}",
        app.transcript()
    );
}

/// The frame at the log's front is replaced in place when the prompt the
/// frontend echoed gives way to the server's own message for the same text.
/// The front slot keeps its name across that, so a block reused on its range
/// alone would carry the turn state its own frames ended with — and the turn
/// the durable message opens, which the following frames carry to its record,
/// would render without a summary row.
#[test]
fn test_replaced_front_rederives_block() {
    let mut app = fresh_app();
    app.push_unanswered_echo("go".into());
    assert_eq!(
        app.transcript().frames().len(),
        1,
        "the log holds the echo alone"
    );
    app.transcript_mut().drop_trailing_echo("go");
    app.transcript_mut().push_frame(user_msg("go"));
    app.transcript_mut()
        .push_frame(TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "pondering the task".into(),
            }),
        )));
    app.transcript_mut()
        .push_frame(agent_msg("here is my answer"));
    app.transcript_mut().push_frame(run_record(2_000));
    app.rebuild_transcript();

    let summary = app.transcript().iter().find_map(|l| match l {
        TranscriptLine::ThoughtFor { reasoning, ms, .. } => Some((reasoning.clone(), *ms)),
        _ => None,
    });
    assert_eq!(
        summary,
        Some((Some("pondering the task".to_string()), Some(2_000))),
        "the turn the replaced frame opened surfaces its summary row: {:?}",
        app.transcript()
    );
}
