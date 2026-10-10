use super::*;

#[test]
fn test_replay_caps_history() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..600 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
        app.rebuild_transcript();
    }
    assert!(app.transcript().len() < 600);
    assert!(
        !app.transcript()
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("msg 0")))
    );
    assert!(
        app.transcript()
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("msg 599")))
    );
}

#[test]
fn test_rebuild_caps_history() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..600 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    assert!(app.transcript().len() < 600);
    assert!(
        !app.transcript()
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("msg 0")))
    );
    assert!(
        app.transcript()
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("msg 599")))
    );
}

#[test]
fn test_small_history_complete() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..10 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    assert!(
        app.transcript()
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("msg 0")))
    );
}

#[test]
fn test_prepend_loads_history() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..600 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    let before = app.transcript().len();
    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));
    assert!(app.transcript().len() > before);
    assert!(
        app.transcript()
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("msg 0")))
    );
}

#[test]
fn test_prepend_skips_tail() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..600 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    let before = app.transcript().len();
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));
    assert_eq!(app.transcript().len(), before);
}

#[test]
fn test_prepend_survives_rebuild() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    for i in 0..100 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("old {i}")));
    }
    app.transcript_mut().push_frame(user_msg("turn boundary"));
    for i in 0..500 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("recent {i}")));
    }
    app.rebuild_transcript();
    assert!(app.sessions.current.loaded_from_frame.get() > 0);
    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));
    assert!(
        app.transcript()
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text.contains("old 1")))
    );
    app.transcript_mut()
        .push_frame(TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "new after prepend".into(),
            }),
        )));
    app.rebuild_transcript();
    assert!(
        app.transcript()
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
        app.transcript_mut()
            .push_frame(user_msg(&format!("old {i}")));
    }
    app.push_unanswered_echo("/model".into());
    for i in 120..700 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("recent {i}")));
    }
    app.rebuild_transcript();
    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));
    let at = |app: &App, want: &str| {
        app.transcript()
            .iter()
            .position(|l| match l {
                TranscriptLine::User(t) => t == want,
                _ => false,
            })
            .unwrap_or_else(|| panic!("row {want:?} in {:?}", app.transcript()))
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
        .transcript()
        .iter()
        .position(|l| matches!(l, TranscriptLine::User(t) if t == "/model"))
        .expect("/model echo present");
    let echo_at = app
        .transcript()
        .iter()
        .position(|l| matches!(l, TranscriptLine::User(t) if t == "go on"))
        .expect("non-slash echo present");
    let agent_done_at = app
        .transcript()
        .iter()
        .position(|l| matches!(l, TranscriptLine::Agent(t) if t == "done"))
        .expect("agent done present");
    assert!(
        slash_model_at < echo_at && echo_at < agent_done_at,
        "echo must be after slash echoes and before agent done: {:?}",
        app.transcript()
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
        .transcript()
        .iter()
        .filter(|l| matches!(l, TranscriptLine::User(t) if t == "go on"))
        .count();
    let results = app.transcript()
        .iter()
        .filter(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if name == "result" && call_id == "c1"))
        .count();
    assert_eq!(
        echoes,
        1,
        "the echo keeps one copy instead of being rendered as the event: {:?}",
        app.transcript()
    );
    assert_eq!(
        results,
        1,
        "the result keeps one copy: {:?}",
        app.transcript()
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
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    app.push_transcript_line(TranscriptLine::Interrupted);
    for i in 10..710 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    let texts: Vec<&str> = app
        .transcript()
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
        !app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::Interrupted)),
        "a notice whose frames left the window is gone: {:?}",
        &app.transcript()[..3]
    );
}

/// A notice the log carries renders once, however many rebuilds follow it:
/// every view derives it from its own frame, so no view keeps a second copy
/// beside it.
#[test]
fn test_log_notice_single_row() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    pump(&mut app, user_msg("go"));
    pump(&mut app, compaction_frame());
    pump(&mut app, agent_msg("ok"));
    let notices = app
        .transcript()
        .iter()
        .filter(|l| matches!(l, TranscriptLine::System(t) if t == "compaction checkpoint"))
        .count();
    assert_eq!(
        notices,
        1,
        "one checkpoint row through a rebuild: {:?}",
        app.transcript()
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
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    app.system_line("debug: logging to /tmp/houyi.log");
    for i in 10..710 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    assert!(
        matches!(app.transcript().first(), Some(TranscriptLine::User(t)) if t == "msg 210"),
        "the window's oldest row heads the transcript: {:?}",
        &app.transcript()[..3]
    );
    assert!(
        !app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(t) if t.starts_with("debug: logging"))),
        "a system line whose frames left the window is gone: {:?}",
        &app.transcript()[..3]
    );
}

/// The row leaving the window is a fact of the drawn view, not only of the
/// rows it keeps: the notice is on screen while its frame is, and the same
/// view stops drawing it once the window has slid past that frame.
#[test]
fn test_notice_undrawn_after_window() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    app.system_line("debug: logging to /tmp/houyi.log");
    let drawn = crate::test_harness::render_text(&app, 80, 40);
    assert!(
        drawn.contains("debug: logging"),
        "the notice is drawn while its frame is in the window: {drawn}"
    );
    for i in 0..710 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
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
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    app.push_unanswered_echo("/model".into());
    for i in 10..710 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    assert!(
        matches!(app.transcript().first(), Some(TranscriptLine::User(t)) if t == "msg 210"),
        "the window's oldest row heads the transcript: {:?}",
        &app.transcript()[..3]
    );
    assert!(
        !app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t == "/model")),
        "an echo whose frames left the window is gone: {:?}",
        &app.transcript()[..3]
    );
}

/// A system line inside the window keeps the position its own frame gives
/// it: below the rows of the frames raised before it, above the rows raised
/// after, and once.
#[test]
fn test_system_row_keeps_place() {
    let mut app = fresh_app();
    app.screen = Screen::Working;
    pump(&mut app, user_msg("msg 0"));
    app.system_line("model set to haiku");
    pump(&mut app, user_msg("msg 1"));
    pump(&mut app, agent_msg("done"));
    let at = |want: &str| {
        app.transcript()
            .iter()
            .position(|l| match l {
                TranscriptLine::User(t) => t == want,
                TranscriptLine::System(t) => t == want,
                _ => false,
            })
            .unwrap_or_else(|| panic!("row {want:?} in {:?}", app.transcript()))
    };
    let (first, sys_at, second) = (at("msg 0"), at("model set to haiku"), at("msg 1"));
    assert!(
        first < sys_at && sys_at < second,
        "the system line sits between the rows around it: {first} < {sys_at} < {second} in {:?}",
        app.transcript()
    );
    let copies = app
        .transcript()
        .iter()
        .filter(|l| matches!(l, TranscriptLine::System(t) if t == "model set to haiku"))
        .count();
    assert_eq!(
        copies,
        1,
        "one system row through the rebuilds: {:?}",
        app.transcript()
    );
}
