//! Tests split out of app.rs so that file stays under the size gate.
//! Child module of app (declared via #[path] in app.rs), so use super::*
//! reaches app private items the same way the inline mod tests did.
use super::*;
use crate::state::{Pane, Stage, TranscriptLine, ViewportMode};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::frontend::{LoginMode, SlashCommand};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn working_app() -> App {
    let mut app = crate::composition::app();
    app.screen = Screen::Working;
    app
}

/// A status-bar drag-select starts a status selection so the chrome text
/// (model/mode/context) can be copied for bug reports. Renders first so
/// the status rect + rows are published by the draw pass, then fires a
/// mouse-down in the status bar and asserts the status surface picked it
/// up (not the transcript or pane surface).
#[test]
fn test_status_bar_click_drags() {
    let mut app = working_app();
    drop(crate::test_support::render_text(&app, 80, 24));
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
    use crate::test_support::render_text;
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
    use crate::test_support::render_text;

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
    app.run_command(SlashCommand::Clear);
    assert_eq!(app.stage, Stage::Idle);
    assert_eq!(app.spec_ctx.step, "idle");
    assert_eq!(app.pane, Pane::Transcript);
    assert_eq!(app.transcript.len(), 1);
}

#[test]
fn test_slash_exit_quits() {
    let mut app = working_app();
    app.run_command(SlashCommand::Exit);
    assert!(app.quit);
}

#[test]
fn test_ctrl_c_quits() {
    let mut app = working_app();
    let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    handle_key(&mut app, k);
    assert!(app.quit);
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
