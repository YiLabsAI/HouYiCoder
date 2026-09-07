//! Terminal lifecycle, event loop, and top-level input routing.
//! Screen-specific behavior remains in the view and key modules.

use crossterm::{
    cursor::{Hide, SetCursorStyle, Show},
    event::{
        self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Frame, Terminal, backend::CrosstermBackend, layout::Rect};
use std::cmp;
use std::error::Error;
use std::io::{self, Write, stdout};
use std::time::{Duration, Instant};

use houyicoder_protocol::frontend::SlashCommand;

use crate::composition::{RunnerBundle, build_app};
use crate::keys;
use crate::notifications::{NotifKind, Notification, copy_toast};
use crate::pending_queue::PendingItem;
use crate::run_control::ClientCommand;
use crate::selection::get_clipboard_path;
use crate::selection::surface::{
    ApprovalSelection, PaneSurface, StatusSurface, Surface, TranscriptSurface, clear_stale_click,
    edge_scroll_if_at_edge, paint_overlay,
};
use crate::state::{App, Pane, Screen, ViewportMode};
use crate::terminal_title::restore;
use crate::view;
use crate::view::working::fleet_pill::{FleetClick, click_route};

/// Run the alternate-screen TUI and restore terminal state on exit.
/// Mouse input belongs to in-app scrolling and selection while active.
type ResumeBuilder = Box<dyn Fn(&str) -> Result<RunnerBundle, Box<dyn Error>>>;

pub fn run_with_runner(
    bundle: RunnerBundle,
    resume_builder: Option<ResumeBuilder>,
) -> io::Result<Option<String>> {
    enable_raw_mode()?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        Hide,
        SetCursorStyle::BlinkingBlock
    )?;
    execute!(stdout(), EnableFocusChange)?;
    assert_mouse_modes()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = build_app(bundle);
    app.startup_handshake(Duration::from_secs(2));
    let mut dirty = true;
    // Tracks the last busy state reported to the terminal emulator via the
    // OSC 9;4 progress sequence, so the escape is written only on transitions.
    let mut reported_busy = false;

    while !app.quit {
        // Apply startup and streaming messages before drawing so a trust prompt
        // never flashes the screen it is about to replace.
        if app.poll_agent() {
            dirty = true;
        }
        // Mirror busy transitions to supporting terminal chrome.
        if app.agent_busy != reported_busy {
            set_terminal_progress(app.agent_busy)?;
            reported_busy = app.agent_busy;
        }
        // Idle drain consumes queued work only after a clean run; interrupted
        // work stays available for editing.
        // Idle frames render only on state changes. Busy frames keep the
        // spinner live without toggling terminal cursor visibility.
        app.idle_drain(resume_builder.as_deref(), &mut dirty);
        // Completed child rows retire after their grace period. The child
        // currently open in teammate view remains pinned.
        let retain_viewed = app.teammate_view.as_ref().map(|v| v.child_sid.as_str());
        if app.fleet.retire_completed(retain_viewed) {
            dirty = true;
        }
        if app.fleet.tick_elapsed(Instant::now()) {
            dirty = true;
        }
        if app.notifications.tick(Instant::now()) {
            dirty = true;
        }
        if dirty || app.agent_busy {
            // Extend projected history before rendering its scroll position.
            app.ensure_projected_above();
            terminal.draw(|f| {
                view::draw(f, &app);
                apply_selection_overlay(f, &app);
            })?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(100))? && handle_event(&mut app, event::read()?)? {
            dirty = true;
        }
        if edge_scroll_if_at_edge(&mut app) {
            dirty = true;
        }
        // Flush accumulated paste chunks after a 50ms gap (paste ended).
        if let (Some(buf), Some(last)) = (app.paste_buffer.take(), app.paste_last.take()) {
            if last.elapsed().as_millis() >= 50 {
                let token = app.pasted.ingest(&buf);
                // Paste into the active palette or input surface.
                app.apply_paste_token(&token);
                dirty = true;
            } else {
                // Still receiving chunks — put back for next iteration.
                app.paste_buffer = Some(buf);
                app.paste_last = Some(last);
            }
        }
    }

    // Clear terminal progress and title state before leaving.
    if reported_busy {
        set_terminal_progress(false)?;
    }
    restore();
    disable_raw_mode()?;
    execute!(
        stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange,
        Show,
        SetCursorStyle::DefaultUserShape
    )?;
    // In-process resume consumes the target. A target survives only when the
    // caller must rebuild the session outside this loop.
    Ok(app.pending_resume_target.take())
}

/// Enable SGR mouse reporting for clicks, drag, hover, and wheel events.
/// Reasserting these idempotent modes recovers from terminal state resets.
fn assert_mouse_modes() -> io::Result<()> {
    let mut out = stdout();
    out.write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h")?;
    out.flush()
}

/// Apply one terminal event and report whether the view changed.
pub(crate) fn handle_event(app: &mut App, event: Event) -> io::Result<bool> {
    match event {
        Event::Key(k) => {
            handle_key(app, k);
            Ok(true)
        }
        Event::Mouse(m) => {
            handle_mouse(app, m);
            Ok(true)
        }
        Event::Paste(data) => {
            // Accumulate chunks (large pastes arrive in multiple Paste
            // events). Flushed in the run loop when the gap exceeds 50ms.
            if let Some(buf) = app.paste_buffer.as_mut() {
                buf.push_str(&data);
            } else {
                app.paste_buffer = Some(data);
            }
            app.paste_last = Some(Instant::now());
            Ok(true)
        }
        Event::Resize(_, _) => {
            // Re-assert the mouse modes alongside the redraw: a resize often
            // accompanies emulator-side state churn (window/tab operations)
            // that can drop DECSET modes.
            assert_mouse_modes()?;
            Ok(true)
        }
        Event::FocusGained => {
            // Focus changes can reset terminal mouse modes; re-enabling them
            // is idempotent.
            assert_mouse_modes()?;
            app.terminal_focused = true;
            Ok(true)
        }
        Event::FocusLost => {
            // Keep the logical caret position while hiding it from an
            // unfocused terminal so input composition resumes correctly.
            app.terminal_focused = false;
            Ok(true)
        }
    }
}

/// Publish indeterminate or cleared progress through OSC 9;4.
fn set_terminal_progress(busy: bool) -> io::Result<()> {
    let mut out = stdout();
    if busy {
        out.write_all(b"\x1b]9;4;3;0\x1b\\")?;
    } else {
        out.write_all(b"\x1b]9;4;0;0\x1b\\")?;
    }
    out.flush()
}

/// Route wheel, click, and drag gestures to the active in-app surface.
/// Releasing a selection copies it through the configured clipboard path.
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
pub(crate) fn handle_mouse(app: &mut App, m: MouseEvent) {
    tracing::debug!(kind = ?m.kind, col = m.column, row = m.row, "mouse event");
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // The jump pill overlays the transcript, so it owns the first
            // hit-test when visible.
            let pill = app.jump_pill_rect.get();
            if pill.width > 0 && pill.height > 0 && in_rect(pill, m.column, m.row) {
                app.scroll_transcript_follow_tail();
                return;
            }
            // Footer queue strip: click a previewed item to recall it into
            // the input box, or click the +N / summary row to pull the whole
            // queue back in order (same as Esc recall).
            let qrect = app.queue_rect.get();
            if qrect.width > 0 && qrect.height > 0 && in_rect(qrect, m.column, m.row) {
                // Match draw_strip's filtered count: items with an empty
                // display are not drawn, so the clickable row count + the
                // overflow threshold must use the same filter.
                let n = app
                    .pending
                    .iter()
                    .filter(|s| !s.display().is_empty())
                    .count();
                if n == 0 {
                    return;
                }
                let row = (m.row - qrect.y) as usize;
                if qrect.height <= 1 {
                    app.pop_queued_to_input();
                    return;
                }
                let shown = if n > 2 { 1 } else { cmp::min(n, 2) } as usize;
                if row < shown {
                    // Map the filtered row back to the pending index:
                    // draw_strip skips items with an empty display, so row N
                    // in the strip is not necessarily pending[N].
                    let Some(idx) = app
                        .pending
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| !s.display().is_empty())
                        .nth(row)
                        .map(|(i, _)| i)
                    else {
                        return;
                    };
                    let item = app.pending.remove(idx);
                    // A recalled Message has a live server copy: drop it over the
                    // wire so a follow-up run does not re-inject it. Parked and
                    // Command have no server copy.
                    if let PendingItem::Message(text) = &item {
                        app.send_cmd(ClientCommand::QueueRemove {
                            session_id: app.session_id.clone(),
                            text: text.clone(),
                        });
                    }
                    // Merge with any in-progress draft (same as Esc recall)
                    // rather than overwriting it.
                    app.merge_recalled_text(item.display().to_string());
                } else {
                    app.pop_queued_to_input();
                }
                return;
            }
            // Pane content owns selection before the transcript beneath it.
            // Each surface manages its own drag lifecycle.
            let prect = app.pane_rect.get();
            if prect.width > 0 && prect.height > 0 && in_rect(prect, m.column, m.row) {
                PaneSurface { app: &mut *app }.handle_down(m.column, m.row);
                return;
            }
            // Status chrome owns selection before the transcript beneath it.
            let srect = app.status_rect.get();
            if srect.width > 0 && srect.height > 0 && in_rect(srect, m.column, m.row) {
                StatusSurface { app: &mut *app }.handle_down(m.column, m.row);
                return;
            }
            // Approval cards retain a local copyable selection.
            let arect = app.approval_rect.get();
            if arect.width > 0 && arect.height > 0 && in_rect(arect, m.column, m.row) {
                ApprovalSelection { app: &mut *app }.handle_down(m.column, m.row);
                return;
            }
            // Fleet hit-testing returns an action before mutating App.
            let frect = app.fleet.rect.get();
            if frect.width > 0 && frect.height > 0 && in_rect(frect, m.column, m.row) {
                let route = click_route(&app.fleet, frect.height, (m.row - frect.y) as usize);
                match route {
                    FleetClick::OpenAgentsPane => app.run_command(SlashCommand::Agents),
                    FleetClick::Select(i) => app.fleet.selected = Some(i),
                    FleetClick::Drill(sid) => {
                        app.enter_teammate_view_for_sid(&sid, true);
                    }
                }
                return;
            }
            let rect = app.transcript_rect.get();
            if in_rect(rect, m.column, m.row) {
                // Byte-window browsing does not share transcript selection
                // coordinates, so clicks do not start a drag.
                if !app.window_mode {
                    TranscriptSurface { app: &mut *app }.handle_down(m.column, m.row);
                }
            } else {
                app.selection.clear();
                app.pane_selection.clear();
                app.status_selection.clear();
                app.approval_selection.clear();
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.pane_selection.is_dragging {
                PaneSurface { app: &mut *app }.handle_drag(m.column, m.row);
            } else if app.status_selection.is_dragging {
                StatusSurface { app: &mut *app }.handle_drag(m.column, m.row);
            } else if app.approval_selection.is_dragging {
                ApprovalSelection { app: &mut *app }.handle_drag(m.column, m.row);
            } else if app.selection.is_dragging {
                TranscriptSurface { app: &mut *app }.handle_drag(m.column, m.row);
            }
        }
        MouseEventKind::Moved => {
            // Motion without a pressed button closes any drag whose release
            // was lost. Multiple surfaces may still carry stale drag state.
            if app.pane_selection.is_dragging {
                PaneSurface { app: &mut *app }.handle_moved();
            }
            if app.status_selection.is_dragging {
                StatusSurface { app: &mut *app }.handle_moved();
            }
            if app.approval_selection.is_dragging {
                ApprovalSelection { app: &mut *app }.handle_moved();
            }
            if app.selection.is_dragging {
                TranscriptSurface { app: &mut *app }.handle_moved();
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if app.pane_selection.is_dragging {
                PaneSurface { app: &mut *app }.handle_up();
                return;
            }
            if app.status_selection.is_dragging {
                StatusSurface { app: &mut *app }.handle_up();
                return;
            }
            if app.approval_selection.is_dragging {
                ApprovalSelection { app: &mut *app }.handle_up();
                return;
            }
            if app.selection.is_dragging {
                TranscriptSurface { app: &mut *app }.handle_up();
            }
        }
        MouseEventKind::ScrollUp => {
            clear_stale_click(&mut app.selection);
            clear_stale_click(&mut app.pane_selection);
            if app.window_mode {
                app.window_scroll.line_up(3);
            } else {
                app.scroll_transcript_line_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            clear_stale_click(&mut app.selection);
            clear_stale_click(&mut app.pane_selection);
            if app.window_mode {
                app.window_scroll.line_down(3);
            } else {
                app.scroll_transcript_line_down(3);
            }
        }
        _ => {}
    }
}

/// True when (x, y) is inside the rect (exclusive of the far edge).
fn in_rect(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

/// Paint each surface selection after layout and before the frame flush.
/// Transcript coordinates include scroll offset; pane coordinates are local.
pub(crate) fn apply_selection_overlay(f: &mut Frame, app: &App) {
    let buf = f.buffer_mut();
    let total = app.transcript_scroll.total.get();
    let scroll_top = app.transcript_scroll.top_offset(total);
    {
        let rows = app.last_transcript_rows.borrow();
        paint_overlay(
            buf,
            app.transcript_rect.get(),
            &rows,
            &app.selection,
            scroll_top,
        );
    }
    {
        let rows = app.last_pane_rows.borrow();
        paint_overlay(buf, app.pane_rect.get(), &rows, &app.pane_selection, 0);
    }
    {
        let rows = app.last_status_rows.borrow();
        paint_overlay(buf, app.status_rect.get(), &rows, &app.status_selection, 0);
    }
    {
        let rows = app.last_approval_rows.borrow();
        paint_overlay(
            buf,
            app.approval_rect.get(),
            &rows,
            &app.approval_selection,
            0,
        );
    }
}

/// Dispatch a key to the right handler based on screen and overlays.
pub(crate) fn handle_key(app: &mut App, k: KeyEvent) {
    if app.pending_trust.is_some() {
        keys::handle_trust(app, k);
        return;
    }
    // ctrl+C copies a selection (with a toast); interrupts a running turn;
    // otherwise no-op. It never quits -- the panic key is not an exit.
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        if app.selection.has_selection() {
            let copied = TranscriptSurface { app: &mut *app }.copy_current();
            if let Some(text) = copied {
                let path = get_clipboard_path();
                app.notifications.add(copy_toast(&text, path));
            }
            return;
        }
        if app.agent_busy {
            app.abort_run();
        }
        return;
    }
    // A second ctrl+D while its short confirmation is visible exits from the
    // base working surface.
    if k.modifiers.contains(KeyModifiers::CONTROL)
        && k.code == KeyCode::Char('d')
        && app.pane == Pane::Transcript
        && app.viewport == ViewportMode::Working
    {
        let pending = app
            .notifications
            .current()
            .is_some_and(|n| n.key == "exit-confirm");
        if pending {
            app.quit = true;
        } else {
            app.notifications.add(Notification::immediate(
                "exit-confirm",
                NotifKind::Text {
                    text: "Press Ctrl+D again to exit".to_string(),
                    color: None,
                },
                Duration::from_millis(800),
            ));
        }
        return;
    }
    // k stops the selected child; a confirmed K stops all children. Esc keeps
    // its non-terminal interrupt role.
    if app.pane == Pane::Transcript
        && app.viewport == ViewportMode::Working
        && app.input.is_empty()
        && app.teammate_view.is_none()
    {
        let running = app
            .fleet
            .entries
            .iter()
            .filter(|e| e.completed.is_none())
            .count();
        if k.code == KeyCode::Char('K') && running > 0 {
            let pending = app
                .notifications
                .current()
                .is_some_and(|n| n.key == "kill-agents-confirm");
            if pending {
                app.notifications.remove("kill-agents-confirm");
                app.send_cmd(ClientCommand::KillAllChildren);
            } else {
                app.notifications.add(Notification::immediate(
                    "kill-agents-confirm",
                    NotifKind::Text {
                        text: format!(
                            "Press K again to stop {running} background agent{}",
                            if running == 1 { "" } else { "s" }
                        ),
                        color: None,
                    },
                    Duration::from_millis(2000),
                ));
            }
            return;
        }
        if k.code == KeyCode::Char('k')
            && running > 0
            && let Some(i) = app.fleet.selected
            && let Some(e) = app.fleet.entries.get(i).filter(|e| e.completed.is_none())
        {
            app.send_cmd(ClientCommand::KillChild {
                child_sid: e.agent_id.clone(),
            });
            return;
        }
        // No valid running selection: fall through so 'k' types.
    }
    match app.screen {
        Screen::Login => keys::handle_login(app, k),
        Screen::Console => keys::handle_console(app, k),
        Screen::Working => keys::handle_working(app, k),
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod app_tests;
