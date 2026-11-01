//! Real-binary journey for background tasks: a long command moves to the
//! background, Down opens the task list, Enter shows its output, k stops it,
//! and Esc walks back out to the transcript. The slash command reaches the
//! same pane.
//!
//! Run via make suite ui (builds the bin first) or
//! cargo test --test ui_all ui_tasks:: -- --ignored after cargo build --bin houyi.

use std::thread;
use std::time::{Duration, Instant};

use crate::common::{Key, PtySession, RENDER_TIMEOUT, pty_session_scripted_env, run_slash_command};

/// A command that keeps printing past the foreground budget, so the background move
/// leaves a live task whose output keeps growing while the detail reads it.
const TICK_SCRIPT: &str = r#"[
  [{"type":"ToolCall","id":"c1","name":"bash","input":{"command":"for i in 1 2 3 4 5 6; do echo tick-$i; sleep 1; done"}}],
  [{"type":"Text","text":"started"}]
]"#;

/// Poll the rendered screen until the predicate holds, so an assertion about
/// a state that has to go away does not read one frame and hope.
fn screen_settles(s: &mut PtySession, mut done: impl FnMut(&str) -> bool) -> bool {
    let deadline = Instant::now() + RENDER_TIMEOUT;
    loop {
        if done(&s.screen().contents()) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// A foreground command that outlives the budget moves to the background on
/// its own: the row says how to reach it, the status bar counts it, and the
/// pane lists, opens, stops, and closes it in one key per level.
#[test]
#[ignore]
fn test_task_list_controls() {
    let budget = houyicoder_config::ENV_HOUYICODER_BASH_FOREGROUND_BUDGET_MS.to_string();
    let mut s = pty_session_scripted_env(TICK_SCRIPT, &[(budget, "300".to_string())]);
    s.send_str("start the ticks");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen("Running in background (task #1", RENDER_TIMEOUT),
        "a command past the budget moves to the background instead of blocking the turn:\n{}",
        s.screen().contents()
    );
    assert!(
        s.wait_for_screen("1 background task", RENDER_TIMEOUT),
        "the status bar counts the running task:\n{}",
        s.screen().contents()
    );

    s.send_key(&Key::Down);
    assert!(
        s.wait_for_screen("Enter to open", RENDER_TIMEOUT),
        "Down on an empty input opens the task list:\n{}",
        s.screen().contents()
    );
    let list = s.screen().contents();
    assert!(
        list.contains("#1") && list.contains("running"),
        "the row carries the number and the state:\n{list}"
    );
    assert!(
        list.contains("echo tick-$i"),
        "the row carries the command:\n{list}"
    );

    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen("Esc to back", RENDER_TIMEOUT),
        "Enter drills into the task:\n{}",
        s.screen().contents()
    );
    assert!(
        s.wait_for_screen("tick-", RENDER_TIMEOUT),
        "the detail shows the command's own output:\n{}",
        s.screen().contents()
    );

    s.send_key(&Key::Char('k'));
    assert!(
        s.wait_for_screen("killed", RENDER_TIMEOUT),
        "k stops the running task and the next read reports how it ended:\n{}",
        s.screen().contents()
    );

    s.send_key(&Key::Esc);
    assert!(
        s.wait_for_screen("Esc to close", RENDER_TIMEOUT),
        "Esc returns one level to the list:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Esc);
    assert!(
        screen_settles(&mut s, |screen| !screen.contains("Esc to close")),
        "Esc at the list closes the pane:\n{}",
        s.screen().contents()
    );

    run_slash_command(&mut s, "tasks");
    assert!(
        s.wait_for_screen("Enter to open", RENDER_TIMEOUT),
        "the slash command opens the same pane:\n{}",
        s.screen().contents()
    );
    assert!(
        s.screen().contents().contains("#1"),
        "the list keeps the task after it ended:\n{}",
        s.screen().contents()
    );
}
