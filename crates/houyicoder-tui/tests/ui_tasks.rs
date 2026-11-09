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

/// A command that ends a moment after moving to the background, then one
/// that keeps running until the test stops it.
const TWO_TASKS_SCRIPT: &str = r#"[
  [{"type":"ToolCall","id":"c1","name":"bash","input":{"command":"for i in 1 2 3; do echo early-$i; sleep 0.4; done"}}],
  [{"type":"ToolCall","id":"c2","name":"bash","input":{"command":"for i in 1 2 3 4 5 6 7 8 9 10; do echo tick-$i; sleep 1; done"}}],
  [{"type":"Text","text":"started"}]
]"#;

/// The task list lines, from its first section heading down to its key hints,
/// so a row-order assertion cannot match transcript text above the pane.
fn pane_lines(screen: &str) -> Option<Vec<&str>> {
    let lines: Vec<&str> = screen.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.contains("Running (") || line.contains("Ended ("))?;
    let end = lines
        .iter()
        .rposition(|line| line.contains("Esc to close"))?;
    (start < end).then(|| lines[start..end].to_vec())
}

/// One command finishes while another keeps running, so the list holds a row
/// of each kind. The running row is listed first, the task that ends crosses
/// into the ended section under the same selection, and reopening the pane
/// lands on its first row whatever the last visit left selected.
#[test]
#[ignore]
fn test_task_list_groups_sections() {
    let budget = houyicoder_config::ENV_HOUYICODER_BASH_FOREGROUND_BUDGET_MS.to_string();
    let mut s = pty_session_scripted_env(TWO_TASKS_SCRIPT, &[(budget, "300".to_string())]);
    s.send_str("run two commands");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen("Running in background (task #2", RENDER_TIMEOUT),
        "the second command is the one still in flight:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Down);
    assert!(
        s.wait_for_screen("Enter to open", RENDER_TIMEOUT),
        "Down opens the task list:\n{}",
        s.screen().contents()
    );
    assert!(
        screen_settles(&mut s, |screen| {
            screen.contains("Running (1)") && screen.contains("Ended (1)")
        }),
        "the finished task moves to the ended section while the other runs:\n{}",
        s.screen().contents()
    );

    let screen = s.screen().contents();
    let lines = pane_lines(&screen).expect("the task list is on screen");
    let at = |needle: &str| lines.iter().position(|line| line.contains(needle));
    let running = at("Running (1)").expect("the running heading");
    let ended = at("Ended (1)").expect("the ended heading");
    let in_flight = at("#2").expect("the running row");
    let finished = at("#1").expect("the ended row");
    assert!(
        running < in_flight && in_flight < ended && ended < finished,
        "the running task is listed above the ended one:\n{screen}"
    );
    assert!(
        lines.iter().any(|line| line.contains("\u{25b6} #2")),
        "the newest task starts out selected:\n{screen}"
    );

    s.send_key(&Key::Char('k'));
    assert!(
        screen_settles(&mut s, |screen| screen.contains("Ended (2)")),
        "the stopped task crosses into the ended section:\n{}",
        s.screen().contents()
    );
    let screen = s.screen().contents();
    let lines = pane_lines(&screen).expect("the task list is on screen");
    let at = |needle: &str| lines.iter().position(|line| line.contains(needle));
    assert!(
        at("#2").expect("the stopped row") < at("#1").expect("the earlier row"),
        "the newest ended task leads its section:\n{screen}"
    );
    assert!(
        lines.iter().any(|line| line.contains("\u{25b6} #2")),
        "the selection follows the task into the ended section:\n{screen}"
    );

    s.send_key(&Key::Down);
    assert!(
        s.wait_for_screen("\u{25b6} #1", RENDER_TIMEOUT),
        "Down moves the selection to the older ended task:\n{}",
        s.screen().contents()
    );

    s.send_key(&Key::Esc);
    assert!(
        screen_settles(&mut s, |screen| !screen.contains("Esc to close")),
        "Esc walks back out to the transcript:\n{}",
        s.screen().contents()
    );
    run_slash_command(&mut s, "tasks");
    assert!(
        s.wait_for_screen("Ended (2)", RENDER_TIMEOUT),
        "the slash command opens the list again:\n{}",
        s.screen().contents()
    );
    assert!(
        s.screen().contents().contains("\u{25b6} #2"),
        "reopening returns the selection to the first row:\n{}",
        s.screen().contents()
    );
}
