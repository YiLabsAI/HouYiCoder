//! Trajectory pane PTY journeys that drive the real binary: the turn list, its
//! paging and key navigation, the history load, and the resume journey
//! observed through the pane. The unit layer asserts pane state; this layer
//! drives the real binary through a real terminal so the rows the user sees
//! decide the outcome.

#![allow(clippy::unwrap_in_result)]

use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use houyicoder_core::SessionEvent;

use crate::common::{
    self, Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir, parse_scroll_status, run_slash_command,
    seed_large_session_on_disk,
};

/// After --resume <sid>, the /trajectory pane lists the seeded turn: the
/// header reads "1 turn" (not "0 turns") + the turn row renders the seeded
/// user input. Discriminates from the transcript-render test: the output is
/// cleared before opening the trajectory pane, so the seeded text can only
/// come from the trajectory turn-list, not the transcript scrollback.
#[test]
#[ignore]
fn test_trajectory_shows_resumed_events() {
    let sessions_dir = fresh_temp_dir("sessions-traj-resume");
    let sid = "12121212-1212-1212-1212-121212121212";
    common::seed_session_on_disk(&sessions_dir, sid, "traj-model", "seeded trajectory prompt");
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after sid resume:\n{}",
        s.output()
    );
    // Isolate the trajectory pane: clear scrollback so the seeded text below
    // can only come from the trajectory turn-list, not the transcript.
    s.clear_output();
    run_slash_command(&mut s, "trajectory");
    // The singular is part of the match, not a separate assertion: a header
    // that dropped the count into a fixed plural phrase renders "1 turns", and
    // the word boundary here rejects it.
    assert!(
        s.wait_for_plain("1 turn ", RENDER_TIMEOUT),
        "trajectory header must report one seeded turn in the singular (not 0, not 1 turns):\n{}",
        s.output_plain()
    );
    assert!(
        s.wait_for_compact("seededtrajectoryprompt", RENDER_TIMEOUT),
        "trajectory turn row must render the seeded user input:\n{}",
        s.output_plain()
    );
}

/// The trajectory list does not wrap. Up from the oldest turn stays there and
/// Down from the newest stays there: a session can hold hundreds of turns, and
/// a wrap would jump the whole history on one keystroke. Driven through the real
/// terminal so the pane's own key routing is what is under test.
#[test]
#[ignore]
fn test_trajectory_no_wrap_nav() {
    let sessions_dir = fresh_temp_dir("sessions-traj-nowrap");
    let sid = "14141414-1414-1414-1414-141414141414";
    common::seed_session_turns_on_disk(
        &sessions_dir,
        sid,
        "traj-model",
        &["first prompt", "second prompt", "third prompt"],
    );
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after sid resume:\n{}",
        s.output()
    );
    s.clear_output();
    run_slash_command(&mut s, "trajectory");
    assert!(
        s.wait_for_plain("3 turns", RENDER_TIMEOUT),
        "trajectory header must report the three seeded turns:\n{}",
        s.output_plain()
    );
    // Home selects the oldest turn, and Up there must not wrap to the newest.
    s.send_key(&Key::Home);
    sleep(Duration::from_millis(200));
    assert!(
        s.screen().contents().contains("▸ T1"),
        "Home selects the oldest turn:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Up);
    sleep(Duration::from_millis(200));
    let screen = s.screen().contents();
    assert!(
        screen.contains("▸ T1"),
        "Up at the oldest turn stays there instead of wrapping to the newest:\n{screen}"
    );
    // End selects the newest, and Down there must not wrap to the oldest.
    s.send_key(&Key::End);
    sleep(Duration::from_millis(200));
    assert!(
        s.screen().contents().contains("▸ T3"),
        "End selects the newest turn:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Down);
    sleep(Duration::from_millis(200));
    let screen = s.screen().contents();
    assert!(
        screen.contains("▸ T3"),
        "Down at the newest turn stays there instead of wrapping to the oldest:\n{screen}"
    );
    drop(s);
}

/// Walking to the top of the loaded window reads the older page behind it: the
/// window widens rather than replacing the tail, and the header stops saying
/// turns are not loaded. Driven through the real terminal, so the pane's own
/// key routing and the worker read are what is under test.
#[test]
#[ignore]
fn test_trajectory_older_loads_page() {
    let sessions_dir = fresh_temp_dir("sessions-traj-older");
    let sid = "16161616-1616-1616-1616-161616161616";
    let prompts: Vec<String> = (0..150).map(|i| format!("older prompt {i}")).collect();
    let refs: Vec<&str> = prompts.iter().map(String::as_str).collect();
    common::seed_session_turns_on_disk(&sessions_dir, sid, "traj-model", &refs);
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after sid resume:\n{}",
        s.output()
    );
    s.clear_output();
    run_slash_command(&mut s, "trajectory");
    // The first page holds the newest hundred turns, so fifty are not loaded.
    assert!(
        s.wait_for_screen("150 turns (50 older not loaded)", RENDER_TIMEOUT),
        "the first page reports the turns it has not read:\n{}",
        s.screen().contents()
    );
    assert!(
        s.screen().contents().contains("older prompt 149"),
        "and it holds the newest turn:\n{}",
        s.screen().contents()
    );
    // Walk to the top of the window, where Up reads the older page.
    for _ in 0..110 {
        s.send_key(&Key::Up);
    }
    let loaded = (0..80).any(|_| {
        sleep(Duration::from_millis(100));
        !s.screen().contents().contains("older not loaded")
    });
    assert!(
        loaded,
        "the older page lands and the header stops hiding turns:\n{}",
        s.screen().contents()
    );
    // The selection keeps the turn it was on across the load, so the window
    // grew above the user rather than pulling the oldest turns into view.
    // Walking on to the top of the widened window reaches the session's first
    // turn.
    let showing_oldest = (0..40).any(|_| {
        for _ in 0..5 {
            s.send_key(&Key::Up);
        }
        sleep(Duration::from_millis(100));
        s.screen().contents().contains("older prompt 0")
    });
    assert!(
        showing_oldest,
        "and the walk reaches the session's oldest turn:\n{}",
        s.screen().contents()
    );
    drop(s);
}

/// Leaving the turn detail returns to the turn the drill opened, and Up at the
/// record list stays on the record list instead of loading older history behind
/// the drill. Driven through the real terminal, so the pane's own key routing
/// and its focus are what is under test.
#[test]
#[ignore]
fn test_trajectory_esc_restores_turn() {
    let sessions_dir = fresh_temp_dir("sessions-traj-esc");
    let sid = "18181818-1818-1818-1818-181818181818";
    common::seed_session_turns_on_disk(
        &sessions_dir,
        sid,
        "traj-model",
        &["first prompt", "second prompt", "third prompt"],
    );
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after sid resume:\n{}",
        s.output()
    );
    s.clear_output();
    run_slash_command(&mut s, "trajectory");
    assert!(
        s.wait_for_plain("3 turns", RENDER_TIMEOUT),
        "trajectory header must report the three seeded turns:\n{}",
        s.output_plain()
    );
    // The pane opens on the tail with the newest turn selected, which is also
    // how the test knows the first page has landed.
    assert!(
        s.wait_for("▸ T3", RENDER_TIMEOUT),
        "the pane opens on the newest turn:\n{}",
        s.screen().contents()
    );

    // Walk up to the second turn and open it.
    s.send_key(&Key::Up);
    sleep(Duration::from_millis(200));
    assert!(
        s.screen().contents().contains("▸ T2"),
        "Up selects the second turn:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Enter);
    sleep(Duration::from_millis(200));
    let detail = s.screen().contents();
    assert!(
        detail.contains("T2"),
        "Enter opens the selected turn:\n{detail}"
    );

    // Up at the record list must not move the turn list behind the drill.
    s.send_key(&Key::Up);
    sleep(Duration::from_millis(200));
    let detail = s.screen().contents();
    assert!(
        detail.contains("T2") && !detail.contains("older not loaded"),
        "Up at the record list stays on the record list:\n{detail}"
    );

    // Esc returns to the list with the turn the drill opened still selected.
    s.send_key(&Key::Esc);
    sleep(Duration::from_millis(200));
    let back = s.screen().contents();
    assert!(
        back.contains("▸ T2"),
        "Esc returns to the turn the drill opened:\n{back}"
    );
    drop(s);
}

/// The lines the seeded reply is made of. The record body carries them one per
/// row, which is how a test reads where the body is scrolled to.
const BODY_LINE_PREFIX: &str = "paged-line-";

/// The key hints each level of the drill states, which is how a test tells
/// which level the pane has drawn without reading the pane's own state.
const TURN_LIST_FOOTER: &str = "Esc to close";
const RECORD_LIST_FOOTER: &str = "Enter to open · Esc to back";
const RECORD_BODY_FOOTER: &str = "Up/Down to scroll · Esc to back";

/// The record-list row the cursor is on, or None while no row carries it.
fn selected_row(screen: &str) -> Option<&str> {
    screen.lines().find(|row| row.contains('▸'))
}

/// The body lines the pane is showing, each as the screen row it sits on and
/// its number, read off the drawn screen: which line the body starts at, and
/// how far what the reader was looking at moved, is what a scroll either did
/// or did not do.
fn body_rows(screen: &str) -> Vec<(usize, usize)> {
    screen
        .lines()
        .enumerate()
        .filter_map(|(row, text)| {
            let at = text.find(BODY_LINE_PREFIX)?;
            let number = text[at + BODY_LINE_PREFIX.len()..]
                .split_whitespace()
                .next()?
                .parse::<usize>()
                .ok()?;
            Some((row, number))
        })
        .collect()
}

/// The first body line the pane is showing.
fn first_body_line(screen: &str) -> Option<usize> {
    body_rows(screen)
        .into_iter()
        .map(|(_, number)| number)
        .min()
}

/// Poll the drawn screen until the predicate holds. A key's effect arrives with
/// the next frame, so waiting on the effect is what keeps a sequence of keys
/// from running ahead of the level each one acts on.
fn wait_until(s: &mut PtySession, holds: impl Fn(&str) -> bool) -> bool {
    for _ in 0..60 {
        if holds(&s.screen().contents()) {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

/// Drill from the turn list to the model record's body: Enter opens the turn,
/// Down walks the cursor down to the model record, Enter opens that record.
/// Each step waits on the drawn screen, so a key cannot act on a level the
/// frame behind it has not drawn yet.
fn open_model_record(s: &mut PtySession) {
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen(RECORD_LIST_FOOTER, RENDER_TIMEOUT),
        "Enter opens the turn's record list:\n{}",
        s.screen().contents()
    );
    // The list opens on the turn's first record. The model call is the last
    // one the turn holds, behind its context line and its tool call.
    let mut on_model = false;
    for _ in 0..3 {
        s.send_key(&Key::Down);
        if wait_until(s, |screen| {
            selected_row(screen).is_some_and(|row| row.contains("model"))
        }) {
            on_model = true;
            break;
        }
    }
    assert!(
        on_model,
        "Down walks the cursor to the model record:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen(RECORD_BODY_FOOTER, RENDER_TIMEOUT),
        "Enter opens the record the cursor is on:\n{}",
        s.screen().contents()
    );
    assert!(
        s.screen().contents().contains("model"),
        "and the record it opened is the model call the cursor was on:\n{}",
        s.screen().contents()
    );
}

/// The reply the journey's model record carries: one numbered line per row, so
/// a test reads where the body is scrolled to off the screen.
fn journey_reply() -> String {
    (1..=40)
        .map(|n| format!("{BODY_LINE_PREFIX}{n:02}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The journey's session. Three turns, and the one in the middle holds a tool
/// record before the model record so the drill has to move the cursor to reach
/// the long body. The turns either side make "Esc returns to the turn it
/// opened" an assertion the pane's default tail position cannot satisfy by
/// accident.
fn journey_events(reply: &str) -> [SessionEvent; 8] {
    [
        SessionEvent::UserInput {
            text: "first prompt".into(),
        },
        SessionEvent::AssistantMessage {
            text: "first reply".into(),
            thinking: None,
        },
        SessionEvent::UserInput {
            text: "second prompt".into(),
        },
        SessionEvent::ToolCall {
            call_id: "call-short".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "true"}),
        },
        SessionEvent::ToolResult {
            call_id: "call-short".into(),
            output: serde_json::json!({"stdout": "short tool body", "exit_code": 0}),
            duration_ms: 5,
        },
        SessionEvent::AssistantMessage {
            text: reply.to_string(),
            thinking: None,
        },
        SessionEvent::UserInput {
            text: "third prompt".into(),
        },
        SessionEvent::AssistantMessage {
            text: "third reply".into(),
            thinking: None,
        },
    ]
}

/// Seed the journey's session, resume it, and leave the /trajectory pane open
/// on the second turn: the turn holding the model record with the long reply.
fn open_journey_pane(sessions_dir: &Path, sid: &str) -> PtySession {
    let events = journey_events(&journey_reply());
    common::seed_session_events(sessions_dir, sid, "traj-model", &events);
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.to_path_buf(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after sid resume:\n{}",
        s.output()
    );
    s.clear_output();
    run_slash_command(&mut s, "trajectory");
    // The pane opens on the newest turn, which is also how the test knows the
    // first page has landed.
    assert!(
        s.wait_for_screen("▸ T3", RENDER_TIMEOUT),
        "the pane opens on the newest turn:\n{}",
        s.screen().contents()
    );
    assert!(
        s.screen().contents().contains(TURN_LIST_FOOTER),
        "the turn list names the keys that act on it:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Up);
    assert!(
        wait_until(&mut s, |screen| screen.contains("▸ T2")),
        "Up selects the second turn:\n{}",
        s.screen().contents()
    );
    s
}

/// A record body taller than the pane opens at its first line, pages ten lines
/// at a time, stops at its last, and the two Esc levels each come back to where
/// the drill started. Driven through the real terminal, so the pane's own key
/// routing and the frame each key acts on are what is under test.
#[test]
#[ignore]
fn test_trajectory_record_body_pages() {
    let sessions_dir = fresh_temp_dir("sessions-traj-record-pages");
    let sid = "1a1a1a1a-1a1a-1a1a-1a1a-1a1a1a1a1a1a";
    let mut s = open_journey_pane(&sessions_dir, sid);

    open_model_record(&mut s);
    assert_eq!(
        first_body_line(&s.screen().contents()),
        Some(1),
        "the body opens at its first line:\n{}",
        s.screen().contents()
    );
    let body = s.screen().contents();
    assert!(
        body.contains(RECORD_BODY_FOOTER),
        "the footer names the keys that act on the body:\n{body}"
    );

    // A page moves the body ten lines: the line at the foot of the first
    // window sits ten rows higher, still on screen.
    let foot = *body_rows(&s.screen().contents())
        .last()
        .expect("a body line on screen");
    s.send_key(&Key::PageDown);
    assert!(
        wait_until(&mut s, |screen| body_rows(screen)
            .iter()
            .any(|(row, number)| *number == foot.1 && *row + 10 == foot.0)),
        "PageDown moves the body ten lines:\n{}",
        s.screen().contents()
    );

    // Walking to the end brings the last line into view, and one more page
    // there changes nothing.
    for _ in 0..8 {
        s.send_key(&Key::PageDown);
    }
    assert!(
        wait_until(&mut s, |screen| screen
            .contains(&format!("{BODY_LINE_PREFIX}40"))),
        "the walk to the end brings the last line into view:\n{}",
        s.screen().contents()
    );
    let bottom = first_body_line(&s.screen().contents());
    s.send_key(&Key::PageDown);
    assert!(
        wait_until(&mut s, |screen| first_body_line(screen) == bottom
            && screen.contains(RECORD_BODY_FOOTER)),
        "one more page at the end changes nothing:\n{}",
        s.screen().contents()
    );

    // Esc returns to the record list on the record the drill opened, then to
    // the turn it opened.
    s.send_key(&Key::Esc);
    assert!(
        wait_until(&mut s, |screen| !screen.contains(RECORD_BODY_FOOTER)
            && selected_row(screen).is_some_and(|row| row.contains("model"))),
        "Esc returns to the record the drill opened:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Esc);
    assert!(
        wait_until(&mut s, |screen| screen.contains("▸ T2")),
        "and again to the turn it opened:\n{}",
        s.screen().contents()
    );

    // The offset belongs to the record, so opening it again starts at the top.
    open_model_record(&mut s);
    assert!(
        wait_until(&mut s, |screen| first_body_line(screen) == Some(1)),
        "the body opens at its first line again:\n{}",
        s.screen().contents()
    );
    drop(s);
}

/// After a big resume, page-up to the top of the loaded rows and then stop
/// sending keys: the history read that loads the rows above must still land, so
/// the status total grows with no key left to wake the loop. The freeze this
/// guards against is the one where the read's delivery waits on an input tick —
/// a reader parked at the boundary never sees older rows.
#[test]
#[ignore]
fn test_history_loads_without_keys() {
    let sessions_dir = fresh_temp_dir("sessions-unfreeze");
    let sid = "44444444-4444-4444-4444-444444444444";
    // Sixteen mebibytes leaves the log far past the resident byte budget, so
    // the descent reaches the top of the loaded rows with older rows still
    // only on disk. Replay time scales with the fixture, so the settle timeout
    // holds headroom for the debug-profile binary.
    let fixture =
        seed_large_session_on_disk(&sessions_dir, sid, "unfreeze-model", 16 * 1024 * 1024);
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    // The replay ends on the tail that carries the settle marker, so reading
    // the screen is the settle signal: polling the accumulated output would
    // copy a multi-megabyte buffer on every pass of a replay this size.
    assert!(
        s.wait_for_screen(&fixture.settle_marker, Duration::from_secs(1500)),
        "replay of {} bytes did not settle:\n{}",
        fixture.log_bytes,
        s.output()
    );

    // Descend with page-ups until the status row reads line 1: the pass that
    // drew the top dispatched a read of the rows above it. The total before
    // that press is the baseline, and a read landing after it takes the total
    // past the baseline, which the deadline below watches for. A press that
    // misses the top within the change window presses again, and the cap
    // bounds the loop for a view that never reaches it.
    let change_window = Duration::from_millis(300);
    let mut reached_top = false;
    let mut baseline = 0;
    let mut first_line: Option<u64> = None;
    for _ in 0..1400 {
        let before = parse_scroll_status(&s.screen().contents());
        let before_total = before.map(|(_, t)| t).unwrap_or(0);
        s.send_key(&Key::PageUp);
        let deadline = Instant::now() + change_window;
        while Instant::now() < deadline {
            sleep(Duration::from_millis(10));
            let now = parse_scroll_status(&s.screen().contents()).map(|(l, _)| l);
            if first_line.is_none() {
                first_line = now;
            }
            if now == Some(1) {
                reached_top = true;
                baseline = before_total;
                break;
            }
            if now != before.map(|(l, _)| l) {
                break;
            }
        }
        if reached_top {
            break;
        }
    }
    let parked = s.screen().contents();
    let (line, total) = parse_scroll_status(&parked).expect("the scroll status row reads");
    // The view starts at the tail, so a descent that began at line 1 would
    // reach the boundary without exercising it; this guard fails the run
    // instead of passing quietly.
    assert!(
        first_line.is_some_and(|l| l > 1),
        "the descent must start below the loaded top (first line {first_line:?}):\n{parked}"
    );
    assert!(
        reached_top,
        "the descent never reached the top of the loaded rows (line {line}):\n{parked}"
    );

    // No more keys. A dispatched read must land on its own, taking the total
    // past the baseline within the deadline; a read whose delivery waits for an
    // input tick leaves the total frozen and this fails.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut landed = false;
    while Instant::now() < deadline {
        sleep(Duration::from_millis(20));
        if let Some((_, total_now)) = parse_scroll_status(&s.screen().contents())
            && total_now > baseline
        {
            landed = true;
            break;
        }
    }
    let final_screen = s.screen().contents();
    assert!(
        landed,
        "no history read landed without keys: total {total}, baseline {baseline}, parked at line {line}. \
         A total that never left the baseline while the view sits at line {line} of {total} loaded rows \
         means the read the last press dispatched never arrived; a descent that stopped short of line 1 \
         would have failed the reached-top check above instead:\n{final_screen}"
    );
    drop(s);
}

/// A turn that switched models keeps that fact visible on a common terminal.
/// The list names the second model as a count and the drill states the switch,
/// so a reader does not have to widen the window to 148 columns to learn that
/// the turn ran on two models.
/// The events of a turn that ran on two models: one tool call, a measured
/// step on each model, and the cache break the switch caused.
fn switch_session_events() -> Vec<SessionEvent> {
    let usage = |turn: u32, model: &str| SessionEvent::TurnUsage {
        turn,
        call_in_turn: 1,
        input_tokens: 100,
        output_tokens: 10,
        cache_read_input_tokens: 90,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        model: model.to_string(),
        recovery: false,
        effort: None,
        input_reported: true,
        cache_read_reported: true,
        cache_write_reported: false,
        cas_recorded: false,
    };
    let timing = |turn: u32, step: u32| SessionEvent::ModelStepTiming {
        turn,
        step,
        total_ms: 900,
        ttft_ms: Some(210),
        decode_ms: Some(600),
        reasoning_ms: None,
        response_ms: None,
    };
    let events = vec![
        SessionEvent::UserInput {
            text: "switched turn".into(),
        },
        SessionEvent::TurnStarted {
            turn: 1,
            call_in_turn: 0,
        },
        SessionEvent::AssistantMessage {
            text: "first reply".into(),
            thinking: None,
        },
        timing(1, 1),
        usage(1, "deepseek-v4-pro-0813"),
        SessionEvent::ToolCall {
            call_id: "call-1".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "true"}),
        },
        SessionEvent::ToolResult {
            call_id: "call-1".into(),
            output: serde_json::json!({"stdout": "ok"}),
            duration_ms: 40,
        },
        SessionEvent::TurnStarted {
            turn: 2,
            call_in_turn: 0,
        },
        SessionEvent::AssistantMessage {
            text: "second reply".into(),
            thinking: None,
        },
        timing(2, 1),
        usage(2, "deepseek-v4.1-flash"),
        SessionEvent::CacheBreak {
            cause: "model-switch".into(),
        },
        SessionEvent::RunCompleted {
            ms: Some(1_000),
            outcome: None,
        },
    ];
    events
}

#[test]
#[ignore]
fn test_trajectory_switch_visible_narrow() {
    let sessions_dir = fresh_temp_dir("sessions-traj-switch");
    let sid = "56565656-5656-5656-5656-565656565656";
    let events = switch_session_events();
    common::seed_session_events(&sessions_dir, sid, "deepseek-v4-pro-0813", &events);
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after sid resume:\n{}",
        s.output()
    );
    // A session switch clears the panes, so a command typed before it lands
    // opens into the outgoing session and is lost. The replayed transcript is
    // the marker that the switch has settled.
    assert!(
        s.wait_for_screen("second reply", RENDER_TIMEOUT),
        "the resumed transcript should replay before the pane opens:\n{}",
        s.screen().contents()
    );
    s.resize(24, 130);
    s.clear_output();
    run_slash_command(&mut s, "trajectory");
    // Neither marker was on screen before the clear above, so the screen
    // cannot satisfy these waits from a stale frame.
    assert!(
        s.wait_for_screen("2 models", RENDER_TIMEOUT),
        "the header must name both models:\nscreen:\n{}\nstream:\n{}",
        s.screen().contents(),
        s.output_plain()
    );
    assert!(
        s.wait_for_screen("+1", RENDER_TIMEOUT),
        "the turn row must keep the switch at 130 columns:\n{}",
        s.screen().contents()
    );
    // The tail of the row is what a column added on the left can push off the
    // edge, so the outcome and the counts are asserted on the same line as the
    // model cell.
    let screen = s.screen().contents();
    let row = screen
        .lines()
        .find(|line| line.contains("+1") && line.contains("✓"))
        .unwrap_or_else(|| panic!("no turn row with the model cell:\n{screen}"));
    assert!(
        row.contains("1 call"),
        "the row keeps its counts at 130 columns: {row:?}"
    );
    assert!(
        !screen.contains('┃'),
        "the list draws no vertical rules: {screen}"
    );

    s.clear_output();
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("-> deepseek-v4.1-flash", RENDER_TIMEOUT),
        "the drill must state the switch:\n{}",
        s.output_plain()
    );
    let detail = s.output_plain();
    assert!(
        detail.contains("models: deepseek-v4-pro-0813 → deepseek-v4.1-flash"),
        "the detail header states the chain:\n{detail}"
    );
    assert!(
        detail.contains("model 1") && detail.contains("TTFT"),
        "the timeline keeps the call ordinal and the wait:\n{detail}"
    );
    assert!(
        !detail.contains('┃'),
        "an instant draws no vertical rule:\n{detail}"
    );
    // A row's tail is what a narrow pane drops, so the model row must still end
    // with its outcome on the real screen, not only in the accumulated output.
    let detail_screen = s.screen().contents();
    let model_row = detail_screen
        .lines()
        .find(|line| line.contains("model 1") && line.contains("TTFT"))
        .unwrap_or_else(|| panic!("no model row on the level 1 screen:\n{detail_screen}"));
    assert!(
        model_row.trim_end().ends_with('✓'),
        "the model row keeps its outcome at 130 columns: {model_row:?}"
    );
    // The third level is where the raw identity is checked: the row above
    // shortens nothing, and the switch names the model it came from.
    s.clear_output();
    for _ in 0..3 {
        s.send_key(&Key::Down);
    }
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("switched", RENDER_TIMEOUT)
            && s.wait_for_plain("deepseek-v4-pro-0813", RENDER_TIMEOUT),
        "the record detail states what the call switched from:\n{}",
        s.output_plain()
    );
}
