//! Tests split out of app.rs so that file stays under the size gate.
//! Child module of app (declared via #[path] in app.rs), so use super::*
//! reaches app private items the same way the inline mod tests did.
use super::*;
use crate::agent_message::{ConnectionEvent, SessionMessage};
use crate::session::{ConnectionStatus, PollOutcome, SessionConnection};
use crate::state::{Pane, Stage, TranscriptLine, ViewportMode};
use crate::test_harness::{
    FailedHandshakeTransport, connected_app_events, connection_lost_app, wait_for_request,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::frontend::{FrontendRequest, LoginMode, SlashCommand};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn working_app() -> App {
    let mut app = crate::composition::app();
    app.screen = Screen::Working;
    app
}

/// The text of the newest transcript system line.
fn last_system(app: &App) -> &str {
    match app.transcript.last().expect("a line was pushed") {
        TranscriptLine::System(t) => t,
        other => panic!("expected a system line, got {other:?}"),
    }
}

/// A status-bar drag-select starts a status selection so the chrome text
/// (model/mode/context) can be copied for bug reports. Renders first so
/// the status rect + rows are published by the draw pass, then fires a
/// mouse-down in the status bar and asserts the status surface picked it
/// up (not the transcript or pane surface).
#[test]
fn test_status_bar_click_drags() {
    let mut app = working_app();
    drop(crate::test_harness::render_text(&app, 80, 24));
    let srect = app.status_rect.get();
    assert!(srect.height > 0, "status rect published by the draw pass");
    let rows = app.last_status_rows.borrow();
    assert!(
        !rows.is_empty(),
        "status rows captured from the frame buffer"
    );
    drop(rows);
    // A left-down inside the status bar routes to StatusSurface, not the
    // transcript surface.
    handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: srect.x,
            row: srect.y,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(
        app.status_selection.is_dragging,
        "click in status bar starts a status drag"
    );
    assert!(
        !app.selection.is_dragging,
        "transcript surface does not grab a status-bar click"
    );
}

/// Switching Working→Scroll→Focus must re-publish the status bar rows so a
/// drag in the current mode copies that mode's text, not stale text from
/// the prior frame. All three status bars sit at the bottom row (y=23 at
/// 24h), so their rects are identical — a rect-only assertion passes while
/// the clipboard holds the prior mode's text. This asserts the row TEXT
/// matches each mode's actual content (Scroll emits a SCROLL tag, Working
/// and Focus emit the progress chain), which is what copy reads. Regression
/// for the stale-rect hole: view::draw zeroes status_rect each frame and
/// each viewport re-publishes via stash_status_rows.
#[test]
fn test_status_rows_refresh() {
    use crate::state::ViewportMode;
    use crate::test_harness::render_text;
    let collect = |app: &App| -> String {
        app.last_status_rows
            .borrow()
            .iter()
            .map(|(_, s)| s.as_str())
            .collect::<Vec<&str>>()
            .join("\n")
    };

    let mut app = working_app();
    drop(render_text(&app, 80, 24));
    let working_rows = collect(&app);
    assert!(
        working_rows.contains("design") || working_rows.contains("verify"),
        "Working status rows hold the progress text: {working_rows}"
    );
    assert!(
        !working_rows.contains("SCROLL"),
        "Working status rows must not carry the Scroll tag: {working_rows}"
    );

    // Scroll re-publishes its own status (line position + SCROLL tag),
    // so the rows must change to the right content, not just any change.
    app.viewport = ViewportMode::Scroll;
    drop(render_text(&app, 80, 24));
    let scroll_rows = collect(&app);
    assert!(
        scroll_rows.contains("SCROLL"),
        "Scroll status rows must carry the SCROLL tag (stale rows = bug): {scroll_rows}"
    );
    assert!(
        app.status_rect.get().height > 0,
        "Scroll re-publishes a status rect, not a stale zero"
    );

    // Focus re-publishes its own status (progress chain + input hint),
    // distinct from Scroll's tag.
    app.viewport = ViewportMode::Focus;
    drop(render_text(&app, 80, 24));
    let focus_rows = collect(&app);
    assert_ne!(
        scroll_rows, focus_rows,
        "Focus status rows must differ from Scroll: {focus_rows}"
    );
    assert!(
        focus_rows.contains("design") || focus_rows.contains("verify"),
        "Focus status rows hold the progress text: {focus_rows}"
    );
    assert!(
        app.status_rect.get().height > 0,
        "Focus re-publishes a status rect"
    );
}

/// A screen with no status bar (Console) must leave status_rect zeroed so
/// a stale rect from the prior Working frame cannot route a drag at empty
/// space. This is the discriminator for the view::draw zeroing at the top
/// of every frame — without it, 1131 tests stay green (the cross-viewport
/// test only covers re-publish, not the zero-when-absent path). Verified
/// red by removing the status_rect.set(0) line: Console render then leaves
/// the stale Working rect Rect{0,23,80,1}.
#[test]
fn test_status_rect_zeroed_offscreen() {
    use crate::state::Screen;
    use crate::test_harness::render_text;

    let mut app = working_app();
    drop(render_text(&app, 80, 24));
    assert!(
        app.status_rect.get().height > 0,
        "Working publishes a status rect"
    );
    // Switch to a screen that draws no status bar. view::draw zeroes
    // status_rect at the frame top; Console never re-publishes, so the
    // rect stays zero — a drag cannot target stale Working chrome.
    app.screen = Screen::Console;
    drop(render_text(&app, 80, 24));
    assert_eq!(
        app.status_rect.get().height,
        0,
        "Console must not keep a stale Working status rect"
    );
}

fn wheel(kind: MouseEventKind) -> MouseEvent {
    MouseEvent {
        kind,
        column: 5,
        row: 5,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn test_wheel_scrolls_in_place() {
    let mut app = working_app();
    for _ in 0..40 {
        app.system_line("a long line of transcript history");
    }
    assert_eq!(app.viewport, ViewportMode::Working);
    assert!(app.transcript_scroll.follow_tail);
    // Wheel up scrolls the transcript in place — it must NOT enter the
    // full-screen Scroll viewport (which would hide the input box).
    handle_mouse(&mut app, wheel(MouseEventKind::ScrollUp));
    assert_eq!(
        app.viewport,
        ViewportMode::Working,
        "wheel must not enter fullscreen Scroll"
    );
    assert!(
        !app.transcript_scroll.follow_tail,
        "wheel-up should detach from the tail"
    );
}

#[test]
fn test_slash_spec_starts_design() {
    let mut app = working_app();
    app.run_command(SlashCommand::Spec);
    assert_eq!(app.stage, Stage::Design);
    assert_eq!(app.pane, Pane::Spec);
}

#[test]
fn test_slash_implement_opens_diff() {
    let mut app = working_app();
    app.run_command(SlashCommand::Implement);
    assert_eq!(app.stage, Stage::Implementing);
    assert_eq!(app.pane, Pane::Diff);
    // /implement no longer raises the tool-approval popup; per-hunk
    // approval happens inline in the diff pane.
    assert!(app.approval.is_none());
}

#[test]
fn test_clear_resets_session() {
    let mut app = working_app();
    app.stage = Stage::Implementing;
    app.pane = Pane::Diff;
    app.spec_ctx.step = "implementing".to_string();
    app.todos.set_cursor(4);
    app.todos.items.push(crate::todo_view::TodoView {
        content: "stale".into(),
        status: crate::todo_view::TodoStatus::Pending,
        active_form: None,
    });
    app.run_command(SlashCommand::Clear);
    assert_eq!(app.stage, Stage::Idle);
    assert_eq!(app.spec_ctx.step, "idle");
    assert_eq!(app.pane, Pane::Transcript);
    assert_eq!(app.transcript.len(), 1);
    assert_eq!(app.todos.cursor(), 0);
    assert!(app.todos.items.is_empty());
}

#[test]
fn test_slash_exit_quits() {
    let mut app = working_app();
    app.run_command(SlashCommand::Exit);
    assert!(app.quit);
}

#[test]
fn test_ctrl_c_idle_noop() {
    let mut app = working_app();
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    );
    assert!(!app.quit, "ctrl+C idle must not quit (interrupt, not exit)");
}

#[test]
fn test_ctrl_d_twice_quits() {
    let mut app = working_app();
    let d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
    handle_key(&mut app, d);
    assert!(!app.quit, "a single ctrl+D must not quit");
    assert!(
        app.notifications
            .current()
            .is_some_and(|n| n.key == "exit-confirm"),
        "the first ctrl+D shows the exit-confirm toast"
    );
    handle_key(&mut app, d);
    assert!(app.quit, "a second ctrl+D within the window quits");
}

#[test]
fn test_q_empty_no_quit() {
    let mut app = working_app();
    crate::keys::handle_working(&mut app, key(KeyCode::Char('q')));
    assert!(!app.quit, "q with empty input must not quit (it types)");
    assert_eq!(app.input.value(), "q");
}

/// An unknown /-prefix (not a known command) is a message to the model,
/// not an "unknown command" error — /-prefixed input goes
/// straight to the model when no command matches.
/// A typo like /nope and a path like /Users/... both flow the same way.
#[test]
fn test_unknown_slash_is_message() {
    let mut app = working_app();
    app.input.set("/nope".to_string());
    app.submit_input();
    let unknown = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("unknown command")));
    assert!(!unknown, "an unknown /-prefix must not error as a command");
    let echoed = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::User(s) if s == "/nope"));
    assert!(echoed, "the unknown /-prefix must echo as a User message");
}

/// Every known slash command leaves a visible User turn before its response.
#[test]
fn test_command_echoes_user_turn() {
    let mut app = working_app();
    app.input.set("/debug".to_string());
    app.submit_input();
    let echoed = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::User(s) if s == "/debug"));
    assert!(
        echoed,
        "/debug must echo as a User turn before its response"
    );
    assert!(
        matches!(app.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("debug")),
        "the debug response should follow the echoed command"
    );
}

/// A leading-slash path (interior slash) is free text, not a command:
/// it must not error as "unknown command" and must echo as a User turn.
#[test]
fn test_slash_path_is_text() {
    let mut app = working_app();
    app.input.set("/home/you/sample-project".to_string());
    app.submit_input();
    let unknown = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("unknown command")));
    assert!(
        !unknown,
        "a leading-slash path must not be parsed as a command"
    );
    let echoed = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::User(s) if s.contains("sample-project")));
    assert!(echoed, "the path must echo as a User turn");
}

/// The viewable scrollback is bounded: once it exceeds the cap the oldest
/// lines are evicted so per-frame render and search stay O(cap), not
/// O(total history). 4000 matches VIEWABLE_SCROLLBACK_CAP in
/// push_transcript_line.
#[test]
fn test_scrollback_evicts_oldest() {
    let mut app = working_app();
    for i in 0..(4000 + 5) {
        app.push_transcript_line(TranscriptLine::User(format!("line-{i}")));
    }
    assert_eq!(
        app.transcript.len(),
        4000,
        "viewable buffer must cap at the scrollback limit"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "line-4004")),
        "the newest line must be kept"
    );
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "line-0")),
        "the oldest line must be evicted"
    );
}

#[test]
fn test_login_sso_via_dispatch() {
    let mut app = crate::composition::app();
    handle_key(&mut app, key(KeyCode::Char('1')));
    assert_eq!(app.screen, Screen::Working);
    assert_eq!(app.login_mode, Some(LoginMode::Sso));
}

/// A fleet with one running child, selected, for the kill tests.
fn running_fleet_app() -> App {
    let mut app = working_app();
    app.fleet.entries.push(crate::agent_message::FleetEntry {
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
    app.fleet.selected = Some(0);
    app
}

/// 'K' (shift+k) two-press kills all running children: first press shows a
/// confirm toast, second within the window sends KillAllChildren.
#[test]
fn test_kill_all_two_press() {
    let mut app = running_fleet_app();
    let big_k = KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT);
    handle_key(&mut app, big_k);
    assert!(
        app.notifications
            .current()
            .is_some_and(|n| n.key == "kill-agents-confirm"),
        "first K shows the confirm toast"
    );
    handle_key(&mut app, big_k);
    assert!(
        app.notifications
            .current()
            .is_none_or(|n| n.key != "kill-agents-confirm"),
        "second K clears the toast and sends the kill"
    );
}

/// 'k' (lowercase) on a selected running child is a single kill, not the
/// two-press kill-all path (no confirm toast).
#[test]
fn test_single_kill_selected() {
    let mut app = running_fleet_app();
    handle_key(&mut app, key(KeyCode::Char('k')));
    assert!(
        app.notifications
            .current()
            .is_none_or(|n| n.key != "kill-agents-confirm"),
        "lowercase k is single kill, not kill-all"
    );
}

/// A single-kill keypress with running children but no selection falls
/// through to typing — the key must not be swallowed when no valid pill is
/// selected. This is the boundary the swallow bug hit (return outside the
/// selection check).
#[test]
fn test_single_kill_without_selection() {
    let mut app = running_fleet_app();
    app.fleet.selected = None;
    handle_key(&mut app, key(KeyCode::Char('k')));
    assert!(
        !app.input.is_empty(),
        "k with no selection types, not swallowed"
    );
    assert!(
        app.notifications.current().is_none(),
        "no toast when no selection"
    );
}

/// 'K' with no running children is a no-op.
#[test]
fn test_kill_all_no_running() {
    let mut app = working_app();
    let big_k = KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT);
    handle_key(&mut app, big_k);
    assert!(
        app.notifications.current().is_none(),
        "K with no running children shows no toast"
    );
}

/// /clear also ships a SessionReset under the current session id so the server
/// zeroes the same session the UI just reset locally.
#[test]
fn test_clear_ships_session_reset() {
    let (mut app, events) = connected_app_events();
    let sid = app.session_id.clone();
    app.run_command(SlashCommand::Clear);
    let req = wait_for_request(&events, |p| {
        matches!(p, FrontendRequest::SessionReset { .. })
    });
    assert_eq!(req.req_id.0, 0, "first request on a fresh session");
    match req.payload {
        FrontendRequest::SessionReset { session_id } => assert_eq!(session_id, sid),
        other => panic!("unexpected request: {other:?}"),
    }
}

/// When the request-id sequence is exhausted, each user command surfaces its
/// own precise "request ids exhausted" line (never the shared "not
/// connected"), and the auto refreshes share one connection-level notice:
/// the first round announces, later rounds stay quiet. The two states stay
/// distinct (#68).
#[test]
fn test_exhausted_commands_report() {
    use houyicoder_protocol::frontend::memory::MemoryToggleWhich;
    let (mut app, _events) = connected_app_events();
    app.session
        .as_ref()
        .expect("connected session")
        .set_next_req_id(u64::MAX);

    // User commands surface a per-command line, never the shared "not connected".
    app.run_command(SlashCommand::Compact);
    assert_eq!(last_system(&app), "compact: request ids exhausted");
    app.run_debug("");
    assert_eq!(last_system(&app), "debug: request ids exhausted");
    app.run_command(SlashCommand::Undo);
    assert_eq!(last_system(&app), "undo: request ids exhausted");
    app.run_memory_subcommand("forget some-key");
    assert_eq!(last_system(&app), "memory: request ids exhausted");
    app.set_model_at_cursor();
    assert_eq!(last_system(&app), "model: request ids exhausted");
    app.tab_cycle_mode();
    assert_eq!(last_system(&app), "permission: request ids exhausted");
    app.toggle_memory_setting(MemoryToggleWhich::Auto);
    assert_eq!(last_system(&app), "memory: request ids exhausted");

    // Auto refreshes share one connection-level notice: the first round
    // announces exactly one line, later rounds stay quiet.
    let before = app.transcript.len();
    app.run_command(SlashCommand::Agents);
    assert_eq!(app.transcript.len(), before + 1);
    assert_eq!(last_system(&app), "request ids exhausted");
    let after_first = app.transcript.len();
    app.run_command(SlashCommand::Model);
    app.run_command(SlashCommand::Context);
    app.run_command(SlashCommand::Tools);
    app.run_command(SlashCommand::Skills);
    app.run_command(SlashCommand::Hooks);
    app.run_command(SlashCommand::Memory);
    app.run_command(SlashCommand::Status);
    assert_eq!(
        app.transcript.len(),
        after_first,
        "later automatic refreshes do not repeat the notice"
    );
}

/// A /clear on an exhausted connection archives the session without
/// repeating the exhaustion notice: the SessionReset refresh is a second auto
/// path and the one-shot flag is already set.
#[test]
fn test_exhausted_clear_no_repeat() {
    let (mut app, _events) = connected_app_events();
    app.session
        .as_ref()
        .expect("connected session")
        .set_next_req_id(u64::MAX);
    app.run_command(SlashCommand::Agents);
    app.run_command(SlashCommand::Clear);
    // Clear archives the transcript, so check the whole post-clear transcript:
    // no exhaustion notice may appear anywhere, and the post-clear line is
    // the session-archived one.
    assert!(
        !app.transcript.iter().any(|line| matches!(
            line,
            TranscriptLine::System(t) if t.contains("request ids exhausted")
        )),
        "clear must not repeat the exhaustion notice"
    );
    match app.transcript.last().expect("a line was pushed") {
        TranscriptLine::System(t) => assert!(
            t.contains("session archived"),
            "the post-clear line archives the session, got {t}"
        ),
        other => panic!("expected the post-clear system line, got {other:?}"),
    }
}

/// App::enqueue returns NotConnected when no active session exists, so a
/// caller distinguishes a missing session from a stopped driver.
#[test]
fn test_enqueue_not_connected() {
    use crate::run_control::ClientCommand;
    use crate::session::EnqueueError;
    use houyicoder_protocol::frontend::SessionId;
    let app = working_app();
    let err = app
        .enqueue(ClientCommand::AbortRun {
            session_id: SessionId::new("s"),
        })
        .unwrap_err();
    assert_eq!(err, EnqueueError::NotConnected);
}

/// App::enqueue returns Closed when the connection object still exists but
/// its command receiver has closed: the command never left this process.
#[test]
fn test_enqueue_closed() {
    use crate::run_control::ClientCommand;
    use crate::session::EnqueueError;
    use crate::test_harness::connection_lost_app;
    use houyicoder_protocol::envelope::RequestId;
    let app = connection_lost_app();
    let err = app
        .enqueue(ClientCommand::StatusQuery {
            req_id: RequestId(0),
        })
        .unwrap_err();
    assert_eq!(err, EnqueueError::Closed);
}

/// Enqueue Ok proves only local queue acceptance. The test does not infer
/// transport delivery or server execution from that result.
#[test]
fn test_enqueue_ok_local_queue() {
    use crate::run_control::ClientCommand;
    use crate::test_harness::connected_app_events;
    use houyicoder_protocol::envelope::RequestId;
    let (app, _events) = connected_app_events();
    assert!(
        app.enqueue(ClientCommand::StatusQuery {
            req_id: RequestId(0)
        })
        .is_ok(),
        "a live connection accepts the command into its local queue"
    );
}

/// The failure line maps each variant to the precise user-facing cause so
/// the not-connected and connection-lost states never fold together.
#[test]
fn test_enqueue_failure_line() {
    use crate::session::EnqueueError;
    use crate::state::App;
    assert_eq!(
        App::enqueue_failure_line("run", EnqueueError::NotConnected),
        "run: not connected"
    );
    assert_eq!(
        App::enqueue_failure_line("run", EnqueueError::Closed),
        "run: connection lost"
    );
}

/// When the driver is gone, user commands each surface their own
/// connection-lost line, while auto refreshes stay silent: the
/// ConnectionLost event is the single visible notice. Every enqueue
/// failure is settled at its call site (#68).
#[test]
fn test_closed_commands_report_loss() {
    use houyicoder_protocol::frontend::memory::MemoryToggleWhich;
    let mut app = connection_lost_app();

    // User commands surface a per-command line, never a silent drop.
    app.run_command(SlashCommand::Compact);
    assert_eq!(last_system(&app), "compact: connection lost");
    app.run_debug("");
    assert_eq!(last_system(&app), "debug: connection lost");
    app.run_command(SlashCommand::Undo);
    assert_eq!(last_system(&app), "undo: connection lost");
    app.run_memory_subcommand("forget some-key");
    assert_eq!(
        last_system(&app),
        "couldn't forget some-key — connection lost"
    );
    app.set_model_at_cursor();
    assert_eq!(last_system(&app), "model: connection lost");
    app.tab_cycle_mode();
    assert_eq!(last_system(&app), "permission: connection lost");
    app.toggle_memory_setting(MemoryToggleWhich::Auto);
    assert_eq!(
        last_system(&app),
        "couldn't toggle auto-memory — connection lost"
    );

    // Auto refreshes never write a loss line: the driver's ConnectionLost
    // event is the single visible notice. The transcript stays unchanged
    // across every auto refresh even though each enqueue returns Closed.
    let before = app.transcript.len();
    app.run_command(SlashCommand::Agents);
    app.run_command(SlashCommand::Model);
    app.run_command(SlashCommand::Context);
    app.run_command(SlashCommand::Tools);
    app.run_command(SlashCommand::Skills);
    app.run_command(SlashCommand::Hooks);
    app.run_command(SlashCommand::Memory);
    app.run_command(SlashCommand::Status);
    assert_eq!(
        app.transcript.len(),
        before,
        "auto refreshes never write a connection-loss line"
    );
}

/// A /clear on a closed connection archives the session without repeating
/// the loss notice: auto paths never write a loss line, so the SessionReset
/// refresh adds nothing to the transcript.
#[test]
fn test_closed_clear_no_repeat() {
    let mut app = connection_lost_app();
    app.run_command(SlashCommand::Agents);
    app.run_command(SlashCommand::Clear);
    // Auto paths never write a loss line (the ConnectionLost event is the
    // sole notice), so the whole post-clear transcript holds no loss line.
    assert!(
        !app.transcript.iter().any(|line| matches!(
            line,
            TranscriptLine::System(t) if t.contains("connection lost")
        )),
        "clear must not add a loss line"
    );
    match app.transcript.last().expect("a line was pushed") {
        TranscriptLine::System(t) => assert!(
            t.contains("session archived"),
            "the post-clear line archives the session, got {t}"
        ),
        other => panic!("expected the post-clear system line, got {other:?}"),
    }
}

/// Auto refreshes never compete with the ConnectionLost event for the single
/// connection-loss notice. The fixture applies the driver's real death first
/// (one notice), so every later auto refresh and closed-channel observation
/// adds nothing.
#[test]
fn test_auto_refresh_silent() {
    let mut app = connection_lost_app();
    // The fixture applied the real death: exactly one loss line landed and
    // the cause is the driver's specific message.
    assert_eq!(
        last_system(&app),
        "agent error: connect failed: no server",
        "the settled loss writes the run-completion error line"
    );
    let after_loss = app.transcript.len();
    assert!(matches!(
        app.connection_status(),
        ConnectionStatus::Lost(ref cause) if !cause.contains("driver stopped")
    ));
    // Later auto refreshes write nothing (enqueue returns Closed, the notice
    // was already settled by the death).
    app.run_command(SlashCommand::Agents);
    app.run_command(SlashCommand::Model);
    assert_eq!(
        app.transcript.len(),
        after_loss,
        "auto refreshes after the settled loss write no second line"
    );
    // The closed channel (a repeat observation) also stays quiet.
    app.poll_agent();
    assert_eq!(
        app.transcript.len(),
        after_loss,
        "a closed channel after the settled loss stays quiet"
    );
}

/// A fresh connection is Connecting until the Hello handshake succeeds. The
/// real driver event drives the transition: poll_startup blocks for the
/// driver's ConnectionReady, which the App applies to reach Ready. A repeat
/// confirmation is idempotent.
#[test]
fn test_connection_status_ready() {
    let (mut app, _events) = connected_app_events();
    assert_eq!(
        app.session.as_ref().expect("session").status(),
        &ConnectionStatus::Connecting,
        "before the handshake result the status is Connecting"
    );
    // Block for the driver's real ConnectionReady and apply it.
    let msg = app
        .session
        .as_mut()
        .expect("session")
        .poll_startup(std::time::Duration::from_secs(5))
        .expect("the handshake result arrives");
    assert!(
        matches!(msg, SessionMessage::Connection(ConnectionEvent::Ready)),
        "the driver announces readiness, got {msg:?}"
    );
    app.handle_agent_message(msg);
    assert_eq!(
        app.connection_status(),
        ConnectionStatus::Ready,
        "the confirmed handshake marks the connection Ready"
    );
    // A repeat confirmation is idempotent: still Ready.
    app.handle_agent_message(SessionMessage::Connection(ConnectionEvent::Ready));
    assert_eq!(app.connection_status(), ConnectionStatus::Ready);
}

/// A late ConnectionReady cannot revive a lost connection: the transition
/// only moves Connecting to Ready, so a confirmation that arrives after the
/// death announcement leaves the status Lost.
#[test]
fn test_late_ready_keeps_lost() {
    let mut app = connection_lost_app();
    assert!(
        matches!(app.connection_status(), ConnectionStatus::Lost(_)),
        "the failed handshake leaves the connection Lost, got {:?}",
        app.connection_status()
    );
    app.handle_agent_message(SessionMessage::Connection(ConnectionEvent::Ready));
    assert!(
        matches!(app.connection_status(), ConnectionStatus::Lost(_)),
        "a late confirmation cannot revive a lost connection"
    );
}

/// The second loss observation neither overwrites the first cause nor
/// repeats the completion: the closed channel after an announced death
/// stays quiet and the specific cause survives.
#[test]
fn test_first_loss_cause_wins() {
    let mut app = connection_lost_app();
    let after_death = app.transcript.len();
    // The closed channel (a second observation after the announced death)
    // must not overwrite the cause or write another line.
    app.poll_agent();
    assert_eq!(
        app.transcript.len(),
        after_death,
        "a closed channel after the announced death stays quiet"
    );
    match app.connection_status() {
        ConnectionStatus::Lost(cause) => assert!(
            !cause.contains("driver stopped"),
            "the first cause survives later observations, got {cause}"
        ),
        other => panic!("expected Lost, got {other:?}"),
    }
}

/// An app with no session reports Disconnected through the unified
/// projection, distinct from every live state.
#[test]
fn test_connection_status_disconnected() {
    let app = working_app();
    assert_eq!(
        app.connection_status(),
        ConnectionStatus::Disconnected,
        "the absent session projects as Disconnected"
    );
}

/// Poll distinguishes an empty channel from a closed one. A live driver's
/// quiet moment is Idle after its handshake message is consumed; a dead
/// driver's exhausted channel is Closed. Folding the two hid dead
/// connections behind a None.
#[test]
fn test_poll_closed_distinct() {
    let mut app = connection_lost_app();
    // The fixture consumed the death announcement; the channel is exhausted.
    let outcome = app.session.as_mut().expect("session").poll();
    assert!(
        matches!(outcome, PollOutcome::Closed),
        "an exhausted channel reports Closed, not Idle: {outcome:?}"
    );
    // A live connection's quiet moment is Idle: block for the driver's real
    // ConnectionReady, then poll the quiet channel.
    let (mut live, _events) = connected_app_events();
    let msg = live
        .session
        .as_mut()
        .expect("session")
        .poll_startup(std::time::Duration::from_secs(5))
        .expect("the handshake result arrives");
    assert!(
        matches!(msg, SessionMessage::Connection(ConnectionEvent::Ready)),
        "the driver announces readiness, got {msg:?}"
    );
    let outcome = live.session.as_mut().expect("session").poll();
    assert!(
        matches!(outcome, PollOutcome::Idle),
        "a live quiet channel reports Idle, not Closed: {outcome:?}"
    );
}

/// A closed channel with no prior death announcement settles the full loss
/// once: the run ends, pending marks sweep, one error line lands, and the
/// connection records the cause. This is the task-panic path, where the
/// announcement never arrived.
#[test]
fn test_closed_settles_loss() {
    let mut app = {
        let runtime = crate::composition::shared_runtime();
        let client = houyicoder_client::Client::new(Box::new(FailedHandshakeTransport));
        let (agent_tx, agent_rx) = std::sync::mpsc::channel();
        let session = SessionConnection::spawn(client, agent_tx, agent_rx, &runtime);
        let mut a = working_app();
        a.runtime = Some(runtime);
        a.session = Some(session);
        a
    };
    // Consume the death announcement WITHOUT applying it — the panic path
    // is the announcement being lost, leaving a closed channel whose status
    // is still Connecting. poll_startup blocks for the driver's death
    // message, so the arrival does not race the assertion.
    let death = app
        .session
        .as_mut()
        .expect("session")
        .poll_startup(std::time::Duration::from_secs(5))
        .expect("the failed handshake reports the death");
    assert!(
        matches!(
            death,
            SessionMessage::Connection(ConnectionEvent::Lost { .. })
        ),
        "the death is readable but treated as lost, got {death:?}"
    );
    let before = app.transcript.len();
    // The driver sent the death and returned, but the runtime may not have
    // dropped the task's agent_tx yet — poll until the channel closes.
    let mut first_changed = false;
    for _ in 0..100 {
        if app.poll_agent() {
            first_changed = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        first_changed,
        "the first closed observation marks state dirty"
    );
    assert_eq!(
        app.transcript.len(),
        before + 1,
        "the closed channel settles one visible loss line"
    );
    assert!(
        matches!(app.connection_status(), ConnectionStatus::Lost(_)),
        "the closed channel records the loss cause"
    );
    assert!(!app.agent_busy(), "the closed channel ends any active run");
    // A repeat poll stays quiet: no second settlement, no line, not dirty.
    let after_loss = app.transcript.len();
    let repeat_changed = app.poll_agent();
    assert!(
        !repeat_changed,
        "a repeat closed observation does not re-mark dirty"
    );
    assert_eq!(
        app.transcript.len(),
        after_loss,
        "a repeat closed observation does not settle again"
    );
}
