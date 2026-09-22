//! Behavioral tests for incremental transcript rebuilding.
//!
//! New frames render immediately. Stable history is reused while the current
//! turn is rebuilt, and rewind, replay, and history loading preserve ordering.

use crate::composition;
use crate::records::{ContextSuggestion, SuggestionSeverity, TranscriptLine};
use crate::state::{App, Screen};
use crate::todo_view::TodoStatus;
use crate::transcript::TranscriptFrame;
use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use serde_json::json;

fn user_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}
fn agent_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}
fn tool_call(id: &str, tool: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, tool)
            .raw_input(json!({}))
            .status(ToolCallStatus::InProgress),
    ))
}
fn tool_result(id: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id,
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(json!({"ok": true})),
    )))
}
fn todo_write_frame(id: &str, todos: &[(&str, &str)]) -> TranscriptFrame {
    let arr = todos
        .iter()
        .map(|(c, s)| json!({"content": c, "status": s}))
        .collect::<Vec<_>>();
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, "todo_write")
            .raw_input(json!({"todos": arr}))
            .status(ToolCallStatus::InProgress),
    ))
}

/// Return an App with empty transcript state.
fn fresh_app() -> App {
    let mut app = composition::app();
    app.transcript.reset();
    app
}

fn pump(app: &mut App, frame: TranscriptFrame) {
    use crate::agent_message::{ServerEvent, SessionMessage};
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(frame)));
}

/// A ToolCall frame renders immediately without waiting for another boundary.
#[test]
fn test_frame_renders_immediately() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "glob"));
    let has_call = app.transcript.iter().any(|l| {
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
        app.transcript
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
    let call_at = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if name != "result" && call_id == "c1"))
        .expect("call row present");
    let result_at = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if name == "result" && call_id == "c1"))
        .expect("result row present");
    assert_eq!(
        result_at,
        call_at + 1,
        "result is adjacent to its call (paired, not orphaned): {:?}",
        app.transcript
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
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::System(_)))
        .expect("system line present");
    let user_at = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::User(_)))
        .expect("user line present");
    assert!(
        sys_at < user_at,
        "system line precedes the run's rows: {:?}",
        app.transcript
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
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::User(t) if t == "/model"))
        .count();
    assert_eq!(
        echoes, 1,
        "the command echo keeps one copy through rebuild: {:?}",
        app.transcript
    );
}
/// A submission in an app with no runner connected is the frontend's own row:
/// nothing carries it to a model, so no reply frame will follow. The log holds
/// it, so a full rebuild re-derives the echo instead of dropping it, and finds
/// no reply row beside it.
#[test]
fn test_submission_echo_one_row() {
    let mut app = fresh_app();
    app.screen = crate::state::Screen::Working;
    app.input.set("hi".to_string());
    app.submit_input();
    app.rebuild_after_frame_edit();
    let echoes = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::User(t) if t == "hi"))
        .count();
    assert_eq!(
        echoes, 1,
        "the submission echo survives a full rebuild: {:?}",
        app.transcript
    );
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Agent(_) | TranscriptLine::Read { .. })),
        "no reply frame, so no reply row: {:?}",
        app.transcript
    );
}
/// A context view refreshed while its own frame sits below the turn boundary
/// still reaches the transcript. The reused rows belong to the frames above
/// that boundary, so a grid whose frame was replaced in place has to be
/// derived again rather than kept as the row it used to be.
#[test]
fn test_context_refresh_after_turn() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    app.push_transcript_line(TranscriptLine::ContextGrid(composition::context_view()));
    app.transcript.push_frame(user_msg("go"));
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
        app.transcript.iter().any(|l| matches!(
            l,
            TranscriptLine::ContextGrid(v)
                if v.suggestions.iter().any(|s| s.title == "refreshed")
        )),
        "the refreshed grid is the row the log derives: {:?}",
        app.transcript.len()
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
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::User(t) if t == "/nope"))
        .count();
    assert_eq!(
        rows, 1,
        "the submitted message keeps one row: {:?}",
        app.transcript
    );
}

#[test]
fn test_rewind_rebuilds_tail() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "glob"));
    pump(&mut app, user_msg("second"));
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("second"))),
        "second user echo present before rewind"
    );
    app.rewind_to_last_user_input();
    let stale = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("second")));
    assert!(
        !stale,
        "rewind dropped the second echo (no stale line): {:?}",
        app.transcript
    );
    let kept = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("go")));
    assert!(
        kept,
        "first user echo survives rewind: {:?}",
        app.transcript
    );
}

/// Rebuilding a frame batch must replace the changing tail without duplicates.
#[test]
fn test_batch_keeps_single() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    // Push two frames directly without an intervening rebuild (a batch), then
    // rebuild once — the incremental path must handle the non-empty suffix.
    app.transcript.push_frame(tool_call("c1", "glob"));
    app.transcript.push_frame(tool_result("c1"));
    app.rebuild_transcript();
    let calls = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if *name != "result" && call_id == "c1"))
        .count();
    let results = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if *name == "result" && call_id == "c1"))
        .count();
    assert_eq!(
        calls, 1,
        "no duplicate call row after batch+rebuild: {:?}",
        app.transcript
    );
    assert_eq!(
        results, 1,
        "no duplicate result row after batch+rebuild: {:?}",
        app.transcript
    );
}

/// todo_write renders through the checklist without adding a tool chip.
#[test]
fn test_todo_skips_chip() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, todo_write_frame("c1", &[("task one", "pending")]));
    let has_tool_row = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::Tool { tool, .. } if *tool == "todo_write"));
    assert!(
        !has_tool_row,
        "todo_write must not render a transcript tool row (the widget owns it): {:?}",
        app.transcript
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
    assert_eq!(app.todos.items.len(), 1);

    pump(&mut app, user_msg("mid-turn note"));

    assert_eq!(app.todos.items.len(), 1);
    assert_eq!(app.todos.items[0].content, "task one");
}

#[test]
fn test_resumed_todo_paused() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("old run"));
    pump(
        &mut app,
        todo_write_frame("c1", &[("unfinished", "in_progress")]),
    );

    assert_eq!(app.todos.items[0].status, TodoStatus::Paused);
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

    assert_eq!(app.todos.items[0].status, TodoStatus::Paused);
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
    assert!(app.todos.completion_at.contains_key("task one"));
    pump(&mut app, user_msg("later"));

    app.rewind_to_last_user_input();

    assert!(app.todos.items.is_empty());
    assert!(app.todos.completion_at.is_empty());
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
    assert!(app.todos.completion_at.contains_key("task one"));
    pump(&mut app, user_msg("later"));

    app.rewind_to_last_user_input();

    assert_eq!(app.todos.items.len(), 2);
    assert!(app.todos.completion_at.is_empty());
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

#[test]
fn test_replay_caps_history() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..600 {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
        app.rebuild_transcript();
    }
    assert!(app.transcript.len() < 600);
    assert!(
        !app.transcript
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("msg 0")))
    );
    assert!(
        app.transcript
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("msg 599")))
    );
}

#[test]
fn test_rebuild_caps_history() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..600 {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    assert!(app.transcript.len() < 600);
    assert!(
        !app.transcript
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("msg 0")))
    );
    assert!(
        app.transcript
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("msg 599")))
    );
}

#[test]
fn test_small_history_complete() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..10 {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    assert!(
        app.transcript
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("msg 0")))
    );
}

#[test]
fn test_prepend_loads_history() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..600 {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    let before = app.transcript.len();
    app.transcript_scroll.jump_to(0);
    app.load_older_frames();
    assert!(app.transcript.len() > before);
    assert!(
        app.transcript
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("msg 0")))
    );
}

#[test]
fn test_prepend_skips_tail() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..600 {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    let before = app.transcript.len();
    app.load_older_frames();
    assert_eq!(app.transcript.len(), before);
}

#[test]
fn test_prepend_survives_rebuild() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..100 {
        app.transcript.push_frame(user_msg(&format!("old {i}")));
    }
    app.transcript.push_frame(user_msg("turn boundary"));
    for i in 0..500 {
        app.transcript.push_frame(user_msg(&format!("recent {i}")));
    }
    app.rebuild_transcript();
    assert!(app.loaded_from_frame.get() > 0);
    app.transcript_scroll.jump_to(0);
    app.load_older_frames();
    assert!(
        app.transcript
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("old 1")))
    );
    app.transcript
        .push_frame(TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "new after prepend".into(),
            }),
        )));
    app.rebuild_transcript();
    assert!(
        app.transcript
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("old 1")))
    );
}

/// A row the frontend raises among older frames rides the batch that loads
/// them: history prepended above it lands the row between the rows of the
/// frames it was raised with, not above the history it belongs inside, and it
/// keeps that place through the rebuild that follows.
#[test]
fn test_prepend_keeps_echo_place() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..120 {
        app.transcript.push_frame(user_msg(&format!("old {i}")));
    }
    app.push_unanswered_echo("/model".into());
    for i in 120..700 {
        app.transcript.push_frame(user_msg(&format!("recent {i}")));
    }
    app.rebuild_transcript();
    app.transcript_scroll.jump_to(0);
    app.load_older_frames();
    let at = |app: &App, want: &str| {
        app.transcript
            .iter()
            .position(|l| match l {
                TranscriptLine::User(t) => t == want,
                _ => false,
            })
            .unwrap_or_else(|| panic!("row {want:?} in {:?}", app.transcript))
    };
    let place = |app: &App| (at(app, "old 119"), at(app, "/model"), at(app, "recent 120"));
    let (before, echo_at, after) = place(&app);
    assert!(
        before < echo_at && echo_at < after,
        "after the batch: the echo sits among the rows it was raised with: \
         {before} < {echo_at} < {after}"
    );
    app.rebuild_transcript();
    let (before, echo_at, after) = place(&app);
    assert!(
        before < echo_at && echo_at < after,
        "after the next rebuild: the echo keeps that place: \
         {before} < {echo_at} < {after}"
    );
}

/// A message the frontend echoes before its own frame reaches the log stays
/// where it was raised, so the tail rebuild must not consume it as another
/// row's rendering: it sits after the command echoes and before the answer.
#[test]
fn test_echo_slash_order() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("run bash"));
    pump(&mut app, tool_call("c1", "bash"));
    pump(&mut app, tool_result("c1"));
    app.push_unanswered_echo("/model".into());
    app.push_unanswered_echo("/memory".into());
    app.push_transcript_line(TranscriptLine::User("go on".into()));
    pump(&mut app, agent_msg("done"));
    let slash_model_at = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::User(t) if t == "/model"))
        .expect("/model echo present");
    let echo_at = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::User(t) if t == "go on"))
        .expect("non-slash echo present");
    let agent_done_at = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::Agent(t) if t == "done"))
        .expect("agent done present");
    assert!(
        slash_model_at < echo_at && echo_at < agent_done_at,
        "echo must be after slash echoes and before agent done: {:?}",
        app.transcript
            .iter()
            .map(|l| format!("{l:?}"))
            .collect::<Vec<_>>()
    );
}

/// An echo pushed while the newest tool call has no result yet. That result
/// frame follows the echo, so the merge must not render the echo as it:
/// both the echoed text and the result survive with one copy each.
#[test]
fn test_echo_survives_result() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("run bash"));
    pump(&mut app, tool_call("c1", "bash"));
    app.push_transcript_line(TranscriptLine::User("go on".into()));
    pump(&mut app, tool_result("c1"));
    let echoes = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::User(t) if t == "go on"))
        .count();
    let results = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if name == "result" && call_id == "c1"))
        .count();
    assert_eq!(
        echoes, 1,
        "the echo keeps one copy instead of being rendered as the event: {:?}",
        app.transcript
    );
    assert_eq!(
        results, 1,
        "the result keeps one copy: {:?}",
        app.transcript
    );
}

/// A log that outgrew the window rebuilds from the frames still covered. A
/// notice whose neighbouring frames left the window leaves with them instead
/// of landing among the rows that remain, where it would head rows of a turn
/// it never belonged to.
#[test]
fn test_slide_drops_notice_row() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..10 {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    app.push_transcript_line(TranscriptLine::Interrupted);
    for i in 10..710 {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    let texts: Vec<&str> = app
        .transcript
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        texts.contains(&"msg 210"),
        "the window renders its oldest covered frame: {:?}",
        &texts[..3]
    );
    assert!(
        !texts.contains(&"msg 209"),
        "a frame the window no longer covers is not rendered"
    );
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Interrupted)),
        "a notice whose frames left the window is gone: {:?}",
        &app.transcript[..3]
    );
}

fn compaction_frame() -> TranscriptFrame {
    TranscriptFrame::Acpx(AcpxNotification::new(
        AcpxMethod::ContextCompactionBoundary,
        json!({ "checkpoint": "01J00000000000000000000000" }),
    ))
}

/// A notice the log carries renders once, however many rebuilds follow it:
/// every view derives it from its own frame, so no view keeps a second copy
/// beside it.
#[test]
fn test_log_notice_single_row() {
    let mut app = fresh_app();
    app.screen = crate::state::Screen::Working;
    pump(&mut app, user_msg("go"));
    pump(&mut app, compaction_frame());
    pump(&mut app, agent_msg("ok"));
    let notices = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::System(t) if t == "compaction checkpoint"))
        .count();
    assert_eq!(
        notices, 1,
        "one checkpoint row through a rebuild: {:?}",
        app.transcript
    );
}

/// A system line raised among frames that later left the window leaves with
/// them. A row the window no longer covers would otherwise drift to the head
/// of the transcript, heading rows of a turn it never belonged to.
#[test]
fn test_system_row_leaves_window() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..10 {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    app.system_line("debug: logging to /tmp/houyi.log");
    for i in 10..710 {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    assert!(
        matches!(app.transcript.first(), Some(TranscriptLine::User(t)) if t == "msg 210"),
        "the window's oldest row heads the transcript: {:?}",
        &app.transcript[..3]
    );
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(t) if t.starts_with("debug: logging"))),
        "a system line whose frames left the window is gone: {:?}",
        &app.transcript[..3]
    );
}

/// The row leaving the window is a fact of the drawn view, not only of the
/// rows it keeps: the notice is on screen while its frame is, and the same
/// view stops drawing it once the window has slid past that frame.
#[test]
fn test_notice_undrawn_after_window() {
    let mut app = fresh_app();
    app.screen = crate::state::Screen::Working;
    app.system_line("debug: logging to /tmp/houyi.log");
    let drawn = crate::test_harness::render_text(&app, 80, 40);
    assert!(
        drawn.contains("debug: logging"),
        "the notice is drawn while its frame is in the window: {drawn}"
    );
    for i in 0..710 {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    let after = crate::test_harness::render_text(&app, 80, 40);
    assert!(
        !after.contains("debug: logging"),
        "the notice is not drawn once its frame left the window: {after}"
    );
    assert!(
        after.contains("msg 709"),
        "the rows the window covers are drawn: {after}"
    );
}

/// A command echo is the frontend's own row as well: a command never reaches
/// the model, so no server frame carries it. It leaves the viewport with the
/// frames it was raised among, like the system line beside it.
#[test]
fn test_echo_row_leaves_window() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..10 {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    app.push_unanswered_echo("/model".into());
    for i in 10..710 {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    assert!(
        matches!(app.transcript.first(), Some(TranscriptLine::User(t)) if t == "msg 210"),
        "the window's oldest row heads the transcript: {:?}",
        &app.transcript[..3]
    );
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t == "/model")),
        "an echo whose frames left the window is gone: {:?}",
        &app.transcript[..3]
    );
}

/// A system line inside the window keeps the position its own frame gives
/// it: below the rows of the frames raised before it, above the rows raised
/// after, and once.
#[test]
fn test_system_row_keeps_place() {
    let mut app = fresh_app();
    app.screen = crate::state::Screen::Working;
    pump(&mut app, user_msg("msg 0"));
    app.system_line("model set to haiku");
    pump(&mut app, user_msg("msg 1"));
    pump(&mut app, agent_msg("done"));
    let at = |want: &str| {
        app.transcript
            .iter()
            .position(|l| match l {
                TranscriptLine::User(t) => t == want,
                TranscriptLine::System(t) => t == want,
                _ => false,
            })
            .unwrap_or_else(|| panic!("row {want:?} in {:?}", app.transcript))
    };
    let (first, sys_at, second) = (at("msg 0"), at("model set to haiku"), at("msg 1"));
    assert!(
        first < sys_at && sys_at < second,
        "the system line sits between the rows around it: {first} < {sys_at} < {second} in {:?}",
        app.transcript
    );
    let copies = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::System(t) if t == "model set to haiku"))
        .count();
    assert_eq!(
        copies, 1,
        "one system row through the rebuilds: {:?}",
        app.transcript
    );
}

/// Trim runs only at the tail. A scrolled-back reader pins a fixed viewport,
/// so draining the oldest lines would shift the content under it; a scroll-up
/// session's loaded older frames must also survive the next rebuild. This
/// floods the live transcript past the cap while scrolled back and asserts the
/// lines accumulate rather than draining.
#[test]
fn test_trim_skips_scrollaway() {
    use crate::scroll::VIEWABLE_SCROLLBACK_CAP;
    let mut app = fresh_app();
    app.screen = crate::state::Screen::Working;
    // Follow the tail: direct pushes trim down to the cap.
    for _ in 0..(VIEWABLE_SCROLLBACK_CAP + 5) {
        app.push_transcript_line(TranscriptLine::Agent("tail line".into()));
    }
    assert_eq!(
        app.transcript.len(),
        VIEWABLE_SCROLLBACK_CAP,
        "follow-tail direct push trims to the cap"
    );
    // Scroll away from the tail, then keep pushing: trim must not drain a
    // scrolled-back reader, so the lines accumulate past the cap.
    app.transcript_scroll.jump_to(0);
    let extra = 50;
    for _ in 0..extra {
        app.push_transcript_line(TranscriptLine::Agent("scrolled-back line".into()));
    }
    assert_eq!(
        app.transcript.len(),
        VIEWABLE_SCROLLBACK_CAP + extra,
        "trim skips while scrolled back, so lines accumulate"
    );
}

/// When trim drains at the tail, the turn boundary shifts with it so the next
/// tail rebuild's stable-prefix slice stays aligned. Before the fix the cap
/// drained underneath an unchanged boundary, leaving it pointing past the
/// prefix the next rebuild would reuse.
#[test]
fn test_trim_shifts_boundary() {
    use crate::scroll::VIEWABLE_SCROLLBACK_CAP;
    let mut app = fresh_app();
    app.screen = crate::state::Screen::Working;
    // A turn boundary with a real prefix: 100 user messages, then an agent
    // frame, rebuild sets line_index to the prefix length (100).
    for i in 0..100 {
        app.transcript.push_frame(user_msg(&format!("prefix {i}")));
    }
    app.transcript.push_frame(agent_msg("turn body"));
    app.rebuild_transcript();
    let boundary_before = app.transcript.current_turn_mut().line_index;
    assert!(boundary_before > 0, "boundary names the prefix length");
    // Flood past the cap at the tail: trim drains, and the boundary shifts
    // down by the dropped count (saturating at zero).
    let over = VIEWABLE_SCROLLBACK_CAP + 10;
    for _ in 0..over {
        app.push_transcript_line(TranscriptLine::Agent("flood".into()));
    }
    assert!(
        app.transcript.current_turn_mut().line_index <= app.transcript.len(),
        "boundary stays within the post-trim transcript length"
    );
    assert!(
        app.transcript.current_turn_mut().line_index < boundary_before,
        "boundary shifted down as the prefix was trimmed"
    );
}

/// The scroll-up white-load: the user scrolled back and loaded older frames;
/// a new frame arriving triggers a rebuild whose exit must not drain the rows
/// just loaded. Before the fix the rebuild exit capped unconditionally and
/// drained the prepended history, so scrolling up was defeated the moment new
/// content arrived.
#[test]
fn test_scrollback_survives_frame() {
    use crate::scroll::VIEWABLE_SCROLLBACK_CAP;
    use crate::transcript::FrontendRow;
    // Enough frames that repeated scroll-up prepends push the transcript
    // past the viewable cap: the frame window keeps the newest rows, and the
    // prepended older rows must push the total past the cap.
    let frame_count = VIEWABLE_SCROLLBACK_CAP + 600;
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..frame_count {
        app.transcript.push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    // Scroll away and load older frames until the history boundary hits zero.
    app.transcript_scroll.jump_to(0);
    while app.loaded_from_frame.get() > 0 {
        app.load_older_frames();
    }
    assert!(
        app.transcript.len() > VIEWABLE_SCROLLBACK_CAP,
        "prepended history pushes the transcript past the cap"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("msg 0"))),
        "oldest loaded row present before the new frame"
    );
    // A new frontend row arrives while scrolled back. The rebuild exit must
    // not trim a scrolled-back reader, so the prepended history survives.
    app.transcript
        .push_frame(TranscriptFrame::Frontend(FrontendRow::System("new".into())));
    app.rebuild_transcript();
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("msg 0"))),
        "scroll-up history survives the frame arrival"
    );
}

/// The incrementally maintained fold cache must agree with a full recompute
/// after an append (tail-only recompute) and after a turn boundary (prefix
/// re-derive). The signature is start, end, key, active; stats, hint, and git
/// ops derive from the same lines, so they cannot diverge once the boundaries
/// match.
fn fold_signature(app: &App) -> Vec<(usize, usize, String, bool)> {
    app.transcript
        .fold_groups()
        .iter()
        .map(|g| (g.start, g.end, g.key.clone(), g.active))
        .collect()
}

fn recompute_fold_signature(app: &App) -> Vec<(usize, usize, String, bool)> {
    crate::fold::compute_fold_groups(app.transcript.lines(), app.agent_busy())
        .iter()
        .map(|g| (g.start, g.end, g.key.clone(), g.active))
        .collect()
}

#[test]
fn test_fold_cache_tracks_recompute() {
    let mut app = fresh_app();
    app.start_run_for_test(0);
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "bash"));
    pump(&mut app, tool_result("c1"));
    pump(&mut app, tool_call("c2", "bash"));
    pump(&mut app, tool_result("c2"));
    assert_eq!(fold_signature(&app), recompute_fold_signature(&app));

    // More pairs in the same turn exercise the incremental tail recompute
    // (the frozen prefix is reused, only the tail rescans).
    pump(&mut app, tool_call("c3", "bash"));
    pump(&mut app, tool_result("c3"));
    assert_eq!(fold_signature(&app), recompute_fold_signature(&app));

    // A new turn moves the boundary, re-derives the prefix, and flips the
    // prior turn's group from active to complete.
    pump(&mut app, user_msg("again"));
    pump(&mut app, tool_call("d1", "bash"));
    pump(&mut app, tool_result("d1"));
    assert_eq!(fold_signature(&app), recompute_fold_signature(&app));

    let sig = fold_signature(&app);
    assert_eq!(sig.len(), 2, "one group per turn");
    assert!(!sig[0].3, "prior turn group completes");
    assert!(sig[1].3, "active turn group stays open");
}

/// A call id reused across turns must not collide in the fold cache. The
/// incremental tail path seeds its per-call-id ordinal from the retained
/// prefix, so turn two's re-emitted c1 continues to c1#1 instead of
/// colliding with turn one's c1#0.
#[test]
fn test_fold_reused_call_id() {
    let mut app = fresh_app();
    app.start_run_for_test(0);
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "bash"));
    pump(&mut app, tool_result("c1"));
    pump(&mut app, user_msg("again"));
    pump(&mut app, tool_call("c1", "bash"));
    pump(&mut app, tool_result("c1"));

    let sig = fold_signature(&app);
    assert_eq!(sig.len(), 2, "one group per turn");
    assert_ne!(sig[0].2, sig[1].2, "reused call id must not collide");
    assert_eq!(sig, recompute_fold_signature(&app));
}

/// The active flag responds to a pause without a rebuild: begin_waiting
/// collapses the active group and end_waiting reopens it, both through the
/// refresh the run-state transitions trigger.
#[test]
fn test_fold_active_tracks_waiting() {
    let mut app = fresh_app();
    app.start_run_for_test(0);
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("w1", "bash"));
    pump(&mut app, tool_result("w1"));
    assert!(fold_signature(&app)[0].3, "running group is active");

    app.run_state.begin_waiting();
    app.refresh_fold_active();
    assert!(!fold_signature(&app)[0].3, "waiting group collapses");

    app.run_state.end_waiting();
    app.refresh_fold_active();
    assert!(fold_signature(&app)[0].3, "resumed group reopens");
}

/// The resident frame log is bounded by bytes: a session long enough to exceed
/// the budget drains its oldest frames instead of holding every frame forever,
/// and the newest rows still render.
#[test]
fn test_evicts_under_byte_budget() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    app.transcript.set_resident_byte_budget(100 * 1024);
    let text = "x".repeat(100);
    for i in 0..1000 {
        app.transcript.push_frame(user_msg(&format!("{i} {text}")));
    }
    app.rebuild_transcript();
    assert!(
        app.transcript.frame_window_start() > 0,
        "frames past the budget are drained"
    );
    assert!(
        app.transcript.resident_bytes() <= app.transcript.resident_byte_budget() as u64,
        "the drain reaches the budget when the window itself fits under it"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.starts_with("999 "))),
        "the newest row renders"
    );
}

/// No block names a frame below the resident front, and the drain keeps the
/// row that opened the active turn: the frames it reclaims belong to turns the
/// window has moved past, so every rendered row still derives from a resident
/// frame.
#[test]
fn test_eviction_stops_at_front() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    app.transcript.set_resident_byte_budget(1024);
    let text = "x".repeat(100);
    for turn in 0..4 {
        app.transcript.push_frame(user_msg(&format!("turn {turn}")));
        for i in 0..300 {
            app.transcript
                .push_frame(agent_msg(&format!("{turn}.{i} {text}")));
        }
    }
    app.rebuild_transcript();
    assert!(
        app.transcript.frame_window_start() > 0,
        "the budget drains the frames of the closed turns"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t == "turn 3")),
        "the row that opened the active turn still renders"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Agent(t) if t.starts_with("3.299 "))),
        "the tail row renders after the drain"
    );
    app.rebuild_transcript();
    for b in app.transcript.blocks().blocks() {
        assert!(
            b.frame_range.start >= app.transcript.frame_window_start(),
            "block {:?} reaches below the resident front",
            b.frame_range
        );
    }
}

/// A rebuild that drains keeps the verdicts of the frames it reclaims: the
/// audit scan runs before the drain, so a frame that leaves the resident window
/// in the same pass is already folded into the cache and stays there.
#[test]
fn test_verdict_survives_drain() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    app.transcript.set_resident_byte_budget(1024);
    app.transcript
        .push_frame(TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextPermissionDecision,
            json!({
                "tool": "bash",
                "verdict": "allow",
                "scope": "once",
                "callId": "c1",
            }),
        )));
    let text = "x".repeat(100);
    for turn in 0..4 {
        app.transcript.push_frame(user_msg(&format!("turn {turn}")));
        for i in 0..300 {
            app.transcript
                .push_frame(agent_msg(&format!("{turn}.{i} {text}")));
        }
    }
    app.rebuild_transcript();
    assert!(
        app.transcript.frame_window_start() > 0,
        "the rebuild drains the frames below the active turn"
    );
    assert_eq!(
        app.verdict_log_cache.len(),
        1,
        "the drained frame's verdict is in the audit cache"
    );
    assert_eq!(app.verdict_log_cache[0].call_id, "c1");
}

/// A log that holds no user frame still reaches the budget: with no turn
/// boundary to protect, the drain takes the oldest frames down to the newest.
/// The window a resumed session reads can start mid-turn, so the bound cannot
/// depend on a user frame being resident.
#[test]
fn test_drain_without_user_frame() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    app.transcript.set_resident_byte_budget(8 * 1024);
    let text = "x".repeat(1000);
    for i in 0..40 {
        app.transcript.push_frame(agent_msg(&format!("{i} {text}")));
    }
    app.rebuild_transcript();
    assert!(
        app.transcript.frame_window_start() > 0,
        "the budget drains a log whose window holds no user frame"
    );
    assert!(
        app.transcript.resident_bytes() <= app.transcript.resident_byte_budget() as u64,
        "the drain reaches the budget: {} > {}",
        app.transcript.resident_bytes(),
        app.transcript.resident_byte_budget()
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Agent(t) if t.starts_with("39 "))),
        "the newest row renders"
    );
}

/// Scrollback loading stops at the resident front: a batch whose frames were
/// drained cannot be re-derived, so the loaded boundary holds at the front
/// rather than naming frames the window cannot read.
#[test]
fn test_scrollback_stops_at_front() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    app.transcript.set_resident_byte_budget(512 * 1024);
    let text = "x".repeat(400);
    for i in 0..2000 {
        app.transcript.push_frame(user_msg(&format!("{i} {text}")));
    }
    app.rebuild_transcript();
    let base = app.transcript.frame_window_start();
    // The clamp binds only when the load reaches past the front while the frame
    // cap still sits above it, which is where a load asks for drained frames.
    let cap = 2000 - 500;
    assert!(
        base > 0 && base + 1 < cap,
        "the drain leaves a loadable range below the frame cap: front={base}"
    );
    app.transcript_scroll.jump_to(0);
    app.loaded_from_frame.set(base + 1);
    app.load_older_frames();
    assert_eq!(
        app.loaded_from_frame.get(),
        base,
        "the loaded boundary stops at the resident front"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.starts_with("1999 "))),
        "the tail survives the load"
    );
}

/// The active turn is what the viewport shows, so the drain keeps it and the
/// row that opened it. A turn whose own bytes exceed the budget holds above it
/// until the turn ends; bounding a single turn is a later step.
#[test]
fn test_active_turn_protected() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    app.transcript.set_resident_byte_budget(1024);
    let text = "x".repeat(200);
    app.transcript.push_frame(user_msg("opening"));
    for i in 0..100 {
        app.transcript.push_frame(agent_msg(&format!("{i} {text}")));
    }
    app.rebuild_transcript();
    assert_eq!(
        app.transcript.frame_window_start(),
        0,
        "the drain keeps the active turn and its opening frame"
    );
    assert!(
        app.transcript.resident_bytes() > app.transcript.resident_byte_budget() as u64,
        "the active turn's own frames stay resident above the budget"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t == "opening")),
        "the opening row still renders"
    );
}

/// A scrollback load lowers the window front below the frame cap. The byte
/// budget still drains afterwards, so a session that reached for older frames
/// is not exempt from the bound.
#[test]
fn test_load_boundary_keeps_draining() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    app.transcript.set_resident_byte_budget(128 * 1024);
    let text = "x".repeat(100);
    for i in 0..600 {
        pump(&mut app, user_msg(&format!("{i} {text}")));
    }
    assert_eq!(
        app.transcript.frame_window_start(),
        0,
        "the budget holds the frames before a load"
    );
    app.transcript_scroll.jump_to(0);
    app.load_older_frames();
    assert!(
        app.loaded_from_frame.get() < 100,
        "the load reaches below the frame cap: {}",
        app.loaded_from_frame.get()
    );
    for i in 600..1000 {
        pump(&mut app, user_msg(&format!("{i} {text}")));
    }
    let base = app.transcript.frame_window_start();
    let bytes = app.transcript.resident_bytes();
    let budget = app.transcript.resident_byte_budget() as u64;
    assert!(base > 0, "the budget drains after a load: base={base}");
    assert!(
        bytes <= budget,
        "resident bytes stay under the budget: {bytes} > {budget}"
    );
}

/// A checklist whose todo-write frame was drained keeps its items: the cursor
/// is an absolute frame index, so eviction neither resets the accumulator nor
/// makes it re-read a frame that is gone.
#[test]
fn test_todo_survives_eviction() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    app.transcript.set_resident_byte_budget(1024);
    let text = "x".repeat(100);
    pump(
        &mut app,
        todo_write_frame("t1", &[("ship the window", "in_progress")]),
    );
    for i in 0..900 {
        pump(&mut app, user_msg(&format!("{i} {text}")));
    }
    assert!(
        app.transcript.frame_window_start() > 0,
        "the todo-write frame was drained"
    );
    assert!(
        app.todos
            .items
            .iter()
            .any(|t| t.content == "ship the window"),
        "the drained frame's checklist survives"
    );
    pump(&mut app, user_msg("tail"));
    assert!(
        app.todos
            .items
            .iter()
            .any(|t| t.content == "ship the window"),
        "the checklist survives the rebuild that follows"
    );
}
