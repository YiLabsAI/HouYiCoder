//! TUI run loop and top-level key dispatch. Owns the App state, pumps
//! crossterm events with a short poll timeout (so terminal resizes redraw),
//! and asks the view module to render. All actions are placeholders: typing a
//! task seeds the transcript, slash commands switch panes or move the stage,
//! and the tool-approval popup is a static placeholder.
//!
//! Per-screen and overlay key handlers live in keys.rs. Command execution
//! (run_command, submit_input, and stage helpers) lives in command.rs as
//! methods on App, so this module depends on keys + state, not the reverse.

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
use std::io::stdout;
use std::time::Duration;

use crate::keys;
use crate::selection::surface::Surface;
use crate::state::{App, Screen};
use crate::view;

/// Run the TUI until the user quits. Restores the terminal on exit.
///
/// Uses the alternate screen buffer so the app owns a clean full-screen
/// surface — prior shell output is not visible during the session. Mouse
/// capture is ON so wheel events reach the app and drive in-app transcript
/// scroll (never the terminal scrollback): scrolling up pages transcript
/// history, not prior shell commands. The app owns selection: drag to select
/// and release auto-copies the selected text via pbcopy (macOS) or OSC 52
/// (remote/SSH) on mouse-up — no Shift-drag, no native terminal selection.
/// Exiting restores the prior screen and releases the mouse.
type ResumeBuilder =
    Box<dyn Fn(&str) -> Result<crate::composition::RunnerBundle, Box<dyn std::error::Error>>>;

pub fn run_with_runner(
    bundle: crate::composition::RunnerBundle,
    resume_builder: Option<ResumeBuilder>,
) -> std::io::Result<Option<String>> {
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
    let mut app = crate::composition::build_app(bundle);
    let mut dirty = true;
    // Tracks the last busy state reported to the terminal emulator via the
    // OSC 9;4 progress sequence, so the escape is written only on transitions.
    let mut reported_busy = false;

    while !app.quit {
        // Report run-in-flight to the terminal emulator itself. iTerm2 and
        // Ghostty support the ConEmu OSC 9;4 progress sequence and render an
        // indeterminate progress shimmer on their own tab-bar edge — chrome
        // outside the TUI cell grid that no in-app drawing can reach. State 3
        // is indeterminate (the light sweep); state 0 removes it.
        if app.agent_busy != reported_busy {
            set_terminal_progress(app.agent_busy)?;
            reported_busy = app.agent_busy;
        }
        // Redraw only when state changed or while the agent is busy (so the
        // spinner animates). Idle frames skip the draw entirely — this stops
        // the cursor from flickering each poll tick (render-on-change, not
        // render-on-timer). ratatui diffs the back buffer and only writes
        // changed cells, then parks the cursor at the declared position, so
        // no per-frame Hide/Show toggle is needed (that itself caused a blink).
        // Idle drain (composition::idle_drain): continuous-state polling +
        // consumptive idempotency. !agent_busy holds every frame; no flood
        // because drain/take CONSUMES the item. A queued item auto-sends on a
        // clean run end (FinalOutput) — the user got their answer, drain FIFO.
        // An interrupt/error parks it for the user to pop + edit (Esc),
        // not auto-fire on a redirect.
        app.idle_drain(resume_builder.as_deref(), &mut dirty);
        // Retire completed footer rows whose grace window elapsed. Runs
        // every poll regardless of agent-busy: a child completes on its own
        // timeline, and the footer should drop its terse done row five
        // seconds later whether the parent is still running or idle. The
        // child the user is drilled into (teammate view) is pinned — its
        // row stays past the grace window so the footer does not drop the
        // child the user is reading; it retires once the user exits.
        let retain_viewed = app.teammate_view.as_ref().map(|v| v.child_sid.as_str());
        if app.fleet.retire_completed(retain_viewed) {
            dirty = true;
        }
        if app.fleet.tick_elapsed(std::time::Instant::now()) {
            dirty = true;
        }
        if app.notifications.tick(std::time::Instant::now()) {
            dirty = true;
        }
        if dirty || app.agent_busy {
            // Progressive prepend: project older frames if the user scrolled
            // to the top of the projected region. Must run before draw (it
            // mutates transcript + scroll, needs &mut App).
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
        if crate::selection::surface::edge_scroll_if_at_edge(&mut app) {
            dirty = true;
        }
        if app.poll_agent() {
            dirty = true;
        }
        // Flush accumulated paste chunks after a 50ms gap (paste ended).
        if let (Some(buf), Some(last)) = (app.paste_buffer.take(), app.paste_last.take()) {
            if last.elapsed().as_millis() >= 50 {
                let token = app.pasted.ingest(&buf);
                // Route the paste to the active input surface: the palette
                // query when the palette is open (so pasting an argument into
                // the hint-after-space popup lands in the query, not the input
                // bar), else the input box.
                app.apply_paste_token(&token);
                dirty = true;
            } else {
                // Still receiving chunks — put back for next iteration.
                app.paste_buffer = Some(buf);
                app.paste_last = Some(last);
            }
        }
    }

    // Always clear the emulator progress state on exit so a busy shimmer
    // never outlives the app in the terminal chrome. Also restore the
    // default tab title so the session name does not outlive the app.
    if reported_busy {
        set_terminal_progress(false)?;
    }
    crate::terminal_title::restore();
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
    // pending_resume_target is set by the session picker (or /resume
    // <id|name|file>) when the user picks a session to switch to. The event
    // loop's try_swap_session consumes it in-process when a resume_builder is
    // wired (the normal path), so this returns None. Some(target) survives
    // only when no builder was wired (degraded fallback): the target is put
    // back + quit set, and the caller may re-enter with a freshly built
    // bundle. None means a clean quit.
    Ok(app.pending_resume_target.take())
}

/// Write the four mouse-tracking DECSET sequences (1000
/// normal + 1002 button/drag + 1003 any/hover + 1006 SGR). NOT 1015 (RXVT):
/// redundant with 1006, and some terminals mis-decode trackpad two-finger
/// scroll as button-1 drag motion under 1015, causing spurious text selection
/// during scroll. The app owns these modes as an invariant: called once at
/// startup and re-asserted on FocusGained and Resize, because a mode dropped
/// by emulator-side churn re-enables native screen-space selection under the
/// app's own (bug-log #30). Idempotent — re-enabling an active mode is a
/// no-op for the terminal.
fn assert_mouse_modes() -> std::io::Result<()> {
    use std::io::Write;
    let mut out = stdout();
    out.write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h")?;
    out.flush()
}

/// Dispatch one terminal event. Returns true when the event dirties the view
/// (a redraw is needed). Extracted from the run loop so the focus-gate path
/// is unit-testable without synthesizing stdin events.
pub(crate) fn handle_event(app: &mut crate::state::App, event: Event) -> std::io::Result<bool> {
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
            app.paste_last = Some(std::time::Instant::now());
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
            // Ownership re-assertion (bug-log #30): the enable sequence was
            // previously written once at startup, so any terminal-state
            // disturbance (suspend/resume, an external process touching the
            // tty, an emulator reset on focus churn) silently dropped mouse
            // reporting. Re-emitting on every focus gain is idempotent and
            // closes that window.
            assert_mouse_modes()?;
            app.terminal_focused = true;
            Ok(true)
        }
        Event::FocusLost => {
            // Gate the input caret on terminal focus (a render-placeholder
            // hides its invert cursor when the window is
            // unfocused). set_cursor_position still parks the hidden cursor
            // at the caret so IME preedit lands correctly on refocus.
            app.terminal_focused = false;
            Ok(true)
        }
    }
}

/// Write the ConEmu OSC 9;4 progress sequence to the terminal. busy=true
/// sets state 3 (indeterminate — the emulator animates a light sweep on its
/// tab-bar edge); busy=false sets state 0 (remove). Terminals without OSC
/// 9;4 support ignore the sequence, so this is safe to emit unconditionally.
fn set_terminal_progress(busy: bool) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = stdout();
    if busy {
        out.write_all(b"\x1b]9;4;3;0\x1b\\")?;
    } else {
        out.write_all(b"\x1b]9;4;0;0\x1b\\")?;
    }
    out.flush()
}

/// Mouse routing: wheel scrolls the transcript in place; a left-button drag
/// inside the transcript rect builds an in-app selection (the terminal's
/// native selection is unavailable under mouse capture). On release the
/// selected text is written to the clipboard (pbcopy / OSC 52) so cmd+C and
/// paste just work. ctrl+C copies a selection instead of quitting.
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
pub(crate) fn handle_mouse(app: &mut App, m: MouseEvent) {
    tracing::debug!(kind = ?m.kind, col = m.column, row = m.row, "mouse event");
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // Jump-to-bottom pill: hit-test before every other surface so a
            // click on the pill returns to the tail instead of starting a
            // drag-selection on the transcript row it overlays (the pill
            // covers the bottom row of transcript_rect). The pill rect is zero
            // (width/height == 0) when hidden, so in_rect never matches then.
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
                let shown = if n > 2 { 1 } else { std::cmp::min(n, 2) } as usize;
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
                    if let crate::pending_queue::PendingItem::Message(text) = &item {
                        app.send_cmd(crate::run_control::ClientCommand::QueueRemove {
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
            // Slash-command pane (/permissions /search /memory): the pane
            // content lives outside the transcript row stash, so it has its
            // own selection surface. A left-down in the pane starts a pane
            // drag; mouse-up copies the pane text. Dispatched before the
            // transcript branch so a click in the pane never starts a
            // transcript selection under it. The gesture lifecycle (recovery,
            // on_click, apply, edge scroll, collapse guard, copy) lives in
            // the surface impl; the router only picks the active surface.
            let prect = app.pane_rect.get();
            if prect.width > 0 && prect.height > 0 && in_rect(prect, m.column, m.row) {
                crate::selection::surface::PaneSurface { app: &mut *app }
                    .handle_down(m.column, m.row);
                return;
            }
            // The status bar is its own chrome row at the bottom of the
            // working surface; a drag here selects the model/mode/context
            // text so it can be copied for bug reports. Routed before the
            // transcript branch so a click in the bar never starts a
            // transcript selection under it.
            let srect = app.status_rect.get();
            if srect.width > 0 && srect.height > 0 && in_rect(srect, m.column, m.row) {
                crate::selection::surface::StatusSurface { app: &mut *app }
                    .handle_down(m.column, m.row);
                return;
            }
            // The approval card is a transient layout slot above the
            // transcript; route a drag here to a card-local selection so
            // the user can copy the command text shown in the prompt.
            let arect = app.approval_rect.get();
            if arect.width > 0 && arect.height > 0 && in_rect(arect, m.column, m.row) {
                crate::selection::surface::ApprovalSelection { app: &mut *app }
                    .handle_down(m.column, m.row);
                return;
            }
            // Fleet strip: the routing is pure (click_route); applying it
            // rides the two existing broad-access points, run_command and
            // enter_teammate_view_for_sid, so no new &mut App fn is born.
            let frect = app.fleet.rect.get();
            if frect.width > 0 && frect.height > 0 && in_rect(frect, m.column, m.row) {
                let route = crate::view::working::fleet_pill::click_route(
                    &app.fleet,
                    frect.height,
                    (m.row - frect.y) as usize,
                );
                match route {
                    crate::view::working::fleet_pill::FleetClick::OpenAgentsPane => {
                        app.run_command(houyicoder_protocol::frontend::SlashCommand::Agents)
                    }
                    crate::view::working::fleet_pill::FleetClick::Select(i) => {
                        app.fleet.selected = Some(i)
                    }
                    crate::view::working::fleet_pill::FleetClick::Drill(sid) => {
                        app.enter_teammate_view_for_sid(&sid, true);
                    }
                }
                return;
            }
            let rect = app.transcript_rect.get();
            if in_rect(rect, m.column, m.row) {
                // Byte-window mode is a read/browse surface: drag-select would
                // map through transcript_scroll (the whole-vec scroll state),
                // which the window view does not own. Selection in the window
                // view is deferred (the 5 scroll consumers stay untouched);
                // for now a click in the window does not start a drag.
                if !app.window_mode {
                    crate::selection::surface::TranscriptSurface { app: &mut *app }
                        .handle_down(m.column, m.row);
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
                crate::selection::surface::PaneSurface { app: &mut *app }
                    .handle_drag(m.column, m.row);
            } else if app.status_selection.is_dragging {
                crate::selection::surface::StatusSurface { app: &mut *app }
                    .handle_drag(m.column, m.row);
            } else if app.approval_selection.is_dragging {
                crate::selection::surface::ApprovalSelection { app: &mut *app }
                    .handle_drag(m.column, m.row);
            } else if app.selection.is_dragging {
                crate::selection::surface::TranscriptSurface { app: &mut *app }
                    .handle_drag(m.column, m.row);
            }
        }
        MouseEventKind::Moved => {
            // Mode-1003 no-button motion while a drag is marked active means
            // the release was lost (pointer left the window, terminal dropped
            // the SGR release). Finish whichever surface is dragging so
            // copy-on-select fires and the stale drag stops following the
            // pointer. Both surfaces are checked — "at most one dragging" is
            // not an invariant (a transcript drag held while the keyboard
            // opens /memory can leave both marked active); a non-dragging
            // surface's finish is a no-op.
            if app.pane_selection.is_dragging {
                crate::selection::surface::PaneSurface { app: &mut *app }.handle_moved();
            }
            if app.status_selection.is_dragging {
                crate::selection::surface::StatusSurface { app: &mut *app }.handle_moved();
            }
            if app.approval_selection.is_dragging {
                crate::selection::surface::ApprovalSelection { app: &mut *app }.handle_moved();
            }
            if app.selection.is_dragging {
                crate::selection::surface::TranscriptSurface { app: &mut *app }.handle_moved();
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if app.pane_selection.is_dragging {
                crate::selection::surface::PaneSurface { app: &mut *app }.handle_up();
                return;
            }
            if app.status_selection.is_dragging {
                crate::selection::surface::StatusSurface { app: &mut *app }.handle_up();
                return;
            }
            if app.approval_selection.is_dragging {
                crate::selection::surface::ApprovalSelection { app: &mut *app }.handle_up();
                return;
            }
            if app.selection.is_dragging {
                crate::selection::surface::TranscriptSurface { app: &mut *app }.handle_up();
            }
        }
        MouseEventKind::ScrollUp => {
            crate::selection::surface::clear_stale_click(&mut app.selection);
            crate::selection::surface::clear_stale_click(&mut app.pane_selection);
            if app.window_mode {
                app.window_scroll.line_up(3);
            } else {
                app.scroll_transcript_line_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            crate::selection::surface::clear_stale_click(&mut app.selection);
            crate::selection::surface::clear_stale_click(&mut app.pane_selection);
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

/// Paint a solid background over the selected cells on each surface. The
/// transcript range walks CONTENT-row space mapped to screen rows through the
/// current scroll offset; the pane range is screen-space (no independent
/// scroll). Non-content rows (spinner, fold summaries, collapse hints) are
/// skipped so they never get a highlight and never pollute copied text.
/// Endpoint columns stay tied to their own rows. Applied after the view draw,
/// before the buffer flushes — the heavy lifting is the shared
/// selection_surface::paint_overlay, called once per surface.
pub(crate) fn apply_selection_overlay(f: &mut Frame, app: &App) {
    let buf = f.buffer_mut();
    let total = app.transcript_scroll.total.get();
    let scroll_top = app.transcript_scroll.top_offset(total);
    {
        let rows = app.last_transcript_rows.borrow();
        crate::selection::surface::paint_overlay(
            buf,
            app.transcript_rect.get(),
            &rows,
            &app.selection,
            scroll_top,
        );
    }
    {
        let rows = app.last_pane_rows.borrow();
        crate::selection::surface::paint_overlay(
            buf,
            app.pane_rect.get(),
            &rows,
            &app.pane_selection,
            0,
        );
    }
    {
        let rows = app.last_status_rows.borrow();
        crate::selection::surface::paint_overlay(
            buf,
            app.status_rect.get(),
            &rows,
            &app.status_selection,
            0,
        );
    }
    {
        let rows = app.last_approval_rows.borrow();
        crate::selection::surface::paint_overlay(
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
    // ctrl+C copies a selection (with a toast); interrupts a running turn;
    // otherwise no-op. It never quits -- the panic key is not an exit.
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        if app.selection.has_selection() {
            let copied =
                crate::selection::surface::TranscriptSurface { app: &mut *app }.copy_current();
            if let Some(text) = copied {
                let path = crate::selection::get_clipboard_path();
                app.notifications
                    .add(crate::notifications::copy_toast(&text, path));
            }
            return;
        }
        if app.agent_busy {
            app.abort_run();
        }
        return;
    }
    // ctrl+D exits on a double press within 800ms. The exit-confirm toast
    // being current is the first-press signal; it expires at the window so
    // a slow second press restarts. Only in base working -- a pane owns the
    // key otherwise. Never clears or deletes the input.
    if k.modifiers.contains(KeyModifiers::CONTROL)
        && k.code == KeyCode::Char('d')
        && app.pane == crate::state::Pane::Transcript
        && app.viewport == crate::state::ViewportMode::Working
    {
        let pending = app
            .notifications
            .current()
            .is_some_and(|n| n.key == "exit-confirm");
        if pending {
            app.quit = true;
        } else {
            app.notifications
                .add(crate::notifications::Notification::immediate(
                    "exit-confirm",
                    crate::notifications::NotifKind::Text {
                        text: "Press Ctrl+D again to exit".to_string(),
                        color: None,
                    },
                    Duration::from_millis(800),
                ));
        }
        return;
    }
    // 'k' on a selected pill kills that one child; 'K' (shift+k) two-press
    // within 2s kills every running child. The k/K family pairs single +
    // all; the two-press window mirrors ctrl+D exit. The pill keeps a
    // selected cursor so no separate selection mode is needed. Esc stays on
    // abort+restore, never kills.
    if app.pane == crate::state::Pane::Transcript
        && app.viewport == crate::state::ViewportMode::Working
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
                app.send_cmd(crate::run_control::ClientCommand::KillAllChildren);
            } else {
                app.notifications
                    .add(crate::notifications::Notification::immediate(
                        "kill-agents-confirm",
                        crate::notifications::NotifKind::Text {
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
            app.send_cmd(crate::run_control::ClientCommand::KillChild {
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
