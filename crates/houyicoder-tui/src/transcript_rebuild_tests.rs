//! Behavioral tests for incremental transcript rebuilding.
//!
//! New frames render immediately. Stable history is reused while the current
//! turn is rebuilt, and rewind, replay, and history loading preserve ordering.

use crate::composition;
use crate::records::TranscriptLine;
use crate::state::App;
use crate::transcript::TranscriptFrame;
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
    app.transcript.clear();
    app.frames.clear();
    app.current_turn_boundary = Default::default();
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

/// A TUI-only system line pushed before a run keeps its position above the
/// frame-derived turn throughout a rebuild. TUI-only lines must not float
/// above or below wire content when the frame log changes.
#[test]
fn test_system_above_wire_turn() {
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
        "system line precedes the wire turn: {:?}",
        app.transcript
    );
}
/// A TUI-only slash-command echo survives a rebuild exactly once. The echo is
/// never in the frame log, so the rebuild preserves the one copy rather than
/// letting a wire turn add a second.
#[test]
fn test_slash_echo_survives_rebuild() {
    let mut app = fresh_app();
    app.push_transcript_line(TranscriptLine::User("/model".into()));
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
    app.frames.push(tool_call("c1", "glob"));
    app.frames.push(tool_result("c1"));
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

    assert_eq!(
        app.todos.items[0].status,
        crate::todo_view::TodoStatus::Paused
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
        app.todos.items[0].status,
        crate::todo_view::TodoStatus::Paused
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
    app.screen = crate::state::Screen::Working;
    for i in 0..600 {
        app.frames.push(user_msg(&format!("msg {i}")));
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
    app.screen = crate::state::Screen::Working;
    for i in 0..600 {
        app.frames.push(user_msg(&format!("msg {i}")));
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
    app.screen = crate::state::Screen::Working;
    for i in 0..10 {
        app.frames.push(user_msg(&format!("msg {i}")));
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
    app.screen = crate::state::Screen::Working;
    for i in 0..600 {
        app.frames.push(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    let before = app.transcript.len();
    app.transcript_scroll.follow_tail = false;
    app.transcript_scroll.offset = 0;
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
    app.screen = crate::state::Screen::Working;
    for i in 0..600 {
        app.frames.push(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    let before = app.transcript.len();
    app.load_older_frames();
    assert_eq!(app.transcript.len(), before);
}

#[test]
fn test_prepend_survives_rebuild() {
    let mut app = fresh_app();
    app.screen = crate::state::Screen::Working;
    for i in 0..100 {
        app.frames.push(user_msg(&format!("old {i}")));
    }
    app.frames.push(user_msg("turn boundary"));
    for i in 0..500 {
        app.frames.push(user_msg(&format!("recent {i}")));
    }
    app.rebuild_transcript();
    assert!(app.loaded_from_frame.get() > 0);
    app.transcript_scroll.follow_tail = false;
    app.transcript_scroll.offset = 0;
    app.load_older_frames();
    assert!(
        app.transcript
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("old 1")))
    );
    app.frames
        .push(TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
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

/// A queued echo that does not start with a slash is not a TUI-only line,
/// so the tail rebuild must not consume it as frame-derived: it stays after
/// the slash echoes and before the answer.
#[test]
fn test_echo_slash_order() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("run bash"));
    pump(&mut app, tool_call("c1", "bash"));
    pump(&mut app, tool_result("c1"));
    app.push_transcript_line(TranscriptLine::User("/model".into()));
    app.push_transcript_line(TranscriptLine::User("/memory".into()));
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
/// TUI-only line whose neighbouring frames left the window must not land among
/// the rows that remain: between two rows of a live turn it reads as a notice
/// belonging to that turn.
#[test]
fn test_slide_keeps_notice_out() {
    let mut app = fresh_app();
    app.screen = crate::state::Screen::Working;
    for i in 0..10 {
        app.frames.push(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    app.push_transcript_line(TranscriptLine::Interrupted);
    for i in 10..710 {
        app.frames.push(user_msg(&format!("msg {i}")));
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
    let notice_at = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::Interrupted))
        .expect("the notice survives the rebuild");
    let first_row = app
        .transcript
        .iter()
        .position(|l| !l.is_tui_only())
        .expect("the window renders rows");
    assert!(
        notice_at < first_row,
        "the notice sits inside the remaining rows at {notice_at} (first row {first_row})"
    );
}
