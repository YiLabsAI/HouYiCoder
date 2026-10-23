//! Terminal lifecycle, event loop, and top-level input routing.
//! Screen-specific behavior remains in the view and key modules.

pub mod state;

use crossterm::{
    cursor::{Hide, MoveTo},
    event::{
        self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
};
use ratatui::{Frame, Terminal, backend::CrosstermBackend, layout::Rect};
use std::cmp;
use std::error::Error;
use std::io::{self, stdout};
use std::time::{Duration, Instant};

use houyicoder_protocol::frontend::SlashCommand;

use crate::composition::{FrontendStartup, RunnerBundle, build_app};
use crate::keys;
use crate::notifications::{NotifKind, Notification, copy_toast};
use crate::run_control::ClientCommand;
use crate::selection::get_clipboard_path;
use crate::selection::region::{
    ApprovalSelection, PaneRegion, Region, RegionIntent, RegionView, StatusRegion,
    TranscriptRegion, clear_stale_click, edge_scroll_if_at_edge, edge_scroll_intent, paint_overlay,
};
use crate::state::{App, Pane, Screen, ViewportMode};
use crate::terminal_guard::{
    CrosstermModes, TerminalGuard, assert_mouse_modes, set_terminal_progress,
};
use crate::view;
use crate::view::working::fleet_strip::{FleetClick, click_route};

/// Run the alternate-screen TUI and restore terminal state on exit.
/// Mouse input belongs to in-app scrolling and selection while active.
type ResumeBuilder = Box<dyn Fn(&str) -> Result<(RunnerBundle, FrontendStartup), Box<dyn Error>>>;

pub fn run_with_runner(
    bundle: RunnerBundle,
    startup: FrontendStartup,
    resume_builder: Option<ResumeBuilder>,
) -> io::Result<Option<String>> {
    // The guard owns every terminal mode change below, so an early error
    // return or a panic unwinding past this loop restores the console
    // instead of leaving it on the alternate screen in raw mode.
    let mut guard = TerminalGuard::acquire(CrosstermModes)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = build_app(bundle, startup);
    app.startup_handshake(Duration::from_secs(2));
    let mut dirty = true;
    // Tracks the last busy state reported to the terminal emulator via the
    // OSC 9;4 progress sequence, so the escape is written only on transitions.
    let mut reported_busy = false;

    while !app.ui.quit {
        // Apply startup and streaming messages before drawing so a trust prompt
        // never flashes the screen it is about to replace.
        if app.poll_agent() {
            dirty = true;
        }
        // Mirror busy transitions to supporting terminal chrome.
        if app.agent_busy() != reported_busy {
            set_terminal_progress(app.agent_busy())?;
            reported_busy = app.agent_busy();
        }
        // Idle drain consumes queued work only after a clean run; interrupted
        // work stays available for editing.
        // Idle frames render only on state changes. Busy frames keep the
        // spinner live without toggling terminal cursor visibility.
        app.idle_drain(resume_builder.as_deref(), &mut dirty);
        if app.drop_completed_fleet() {
            dirty = true;
        }
        let now = Instant::now();
        if app.tick_fleet_elapsed(now) {
            dirty = true;
        }
        if app.ui.notifications.tick(now) {
            dirty = true;
        }
        if app.sessions.current.todos.prune(now) {
            dirty = true;
        }
        // Drain a background history read outside the dirty gate: an idle
        // loop still picks the result up, and a landed result sets dirty so
        // the redraw that renders the prepended rows follows immediately.
        if app.pump_background_reads() {
            dirty = true;
        }
        if dirty || app.agent_busy() {
            // Load older history before rendering its scroll position.
            app.load_older_frames();
            // Stage the frame: the draw writes into a fresh value, and the
            // last published frame is held aside until the draw reports
            // success.
            // The staged frame starts empty, so the parked cursor position
            // needs no reset here.
            let published = app.ui.stage_frame();
            let drawn = terminal.draw(|f| {
                view::draw(f, &app);
                apply_selection_overlay(f, &app);
            });
            // A failed draw puts the last published frame back before the
            // error unwinds to the terminal guard, so nothing reads a
            // half-written one.
            app.ui.settle_frame(published, drawn)?;
            if let Some((x, y)) = app.ui.frame.feedback.native_cursor_position.get() {
                execute!(stdout(), MoveTo(x, y), Hide)?;
            }
            dirty = false;
        }
        if event::poll(Duration::from_millis(100))? && handle_event(&mut app, event::read()?)? {
            dirty = true;
        }
        if edge_scroll_if_at_edge(&mut app) {
            dirty = true;
        }
        // Flush accumulated paste chunks after a 50ms gap (paste ended).
        if app.flush_pending_paste() {
            dirty = true;
        }
    }

    // Restore the terminal modes the guard owns. The tab title and the
    // progress indicator are restored inside the guard's teardown, so this is
    // the one teardown step the loop has to drive explicitly.
    guard.release();
    // In-process resume consumes the /resume Command during the swap. A
    // command survives only when the caller must rebuild the session outside
    // this loop (no in-process builder), so hand its target up.
    Ok(app.ui.input.queue.drain_resume_target())
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
            if let Some(buf) = app.ui.input.pending_paste.buffer.as_mut() {
                buf.push_str(&data);
            } else {
                app.ui.input.pending_paste.buffer = Some(data);
            }
            app.ui.input.pending_paste.last = Some(Instant::now());
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
            app.ui.terminal_focused = true;
            Ok(true)
        }
        Event::FocusLost => {
            // Keep the logical caret position while hiding it from an
            // unfocused terminal so input composition resumes correctly.
            app.ui.terminal_focused = false;
            Ok(true)
        }
    }
}

/// Route wheel, click, and drag gestures to the active in-app surface.
/// Releasing a selection copies it through the configured clipboard path.
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
pub(crate) fn handle_mouse(app: &mut App, m: MouseEvent) {
    tracing::debug!(kind = ?m.kind, col = m.column, row = m.row, "mouse event");
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // The jump-to-bottom label overlays the transcript, so it owns the
            // first hit-test when visible.
            let rect = app.ui.frame.layout.jump_to_bottom_rect.get();
            if rect.width > 0 && rect.height > 0 && in_rect(rect, m.column, m.row) {
                app.scroll_transcript_follow_tail();
                return;
            }
            // Footer queue strip: click a previewed item to recall it into
            // the input box, or click the +N / summary row to open the queue
            // pane (non-destructive — does not recall all items).
            let qrect = app.ui.frame.layout.queue_rect.get();
            if qrect.width > 0 && qrect.height > 0 && in_rect(qrect, m.column, m.row) {
                let n = app
                    .ui
                    .input
                    .queue
                    .iter()
                    .filter(|s| !s.display().is_empty())
                    .count();
                if n == 0 {
                    return;
                }
                let row = (m.row - qrect.y) as usize;
                if qrect.height <= 1 {
                    app.ui.navigation.pane = Pane::Queue;
                    app.ui.input.queue.select_first();
                    return;
                }
                let shown = if n > 2 { 1 } else { cmp::min(n, 2) } as usize;
                if row < shown {
                    let Some(idx) = app
                        .ui
                        .input
                        .queue
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| !s.display().is_empty())
                        .nth(row)
                        .map(|(i, _)| i)
                    else {
                        return;
                    };
                    let Some(item) = app.remove_pending_at(idx) else {
                        return;
                    };
                    app.ui.input.merge_recalled_text(item.display().to_string());
                } else {
                    app.ui.navigation.pane = Pane::Queue;
                    app.ui.input.queue.select_first();
                }
                return;
            }
            // Pane content owns selection before the transcript beneath it.
            // Each surface manages its own drag lifecycle.
            let prect = app.ui.frame.layout.pane_rect.get();
            if prect.width > 0 && prect.height > 0 && in_rect(prect, m.column, m.row) {
                let intent = PaneRegion {
                    view: app.region_view(),
                }
                .handle_down(m.column, m.row);
                app.apply_region_intent(intent);
                return;
            }
            // Status chrome owns selection before the transcript beneath it.
            let srect = app.ui.frame.layout.status_rect.get();
            if srect.width > 0 && srect.height > 0 && in_rect(srect, m.column, m.row) {
                let intent = StatusRegion {
                    view: app.region_view(),
                }
                .handle_down(m.column, m.row);
                app.apply_region_intent(intent);
                return;
            }
            // Approval cards retain a local copyable selection.
            let arect = app.ui.frame.layout.approval_rect.get();
            if arect.width > 0 && arect.height > 0 && in_rect(arect, m.column, m.row) {
                let intent = ApprovalSelection {
                    view: app.region_view(),
                }
                .handle_down(m.column, m.row);
                app.apply_region_intent(intent);
                return;
            }
            // Fleet hit-testing returns an action before mutating App.
            let frect = app.ui.frame.layout.fleet_rect.get();
            if frect.width > 0 && frect.height > 0 && in_rect(frect, m.column, m.row) {
                let entries = &app.sessions.current.capabilities.fleet.entries;
                let selected = app.ui.fleet.selected();
                let route =
                    click_route(entries, selected, frect.height, (m.row - frect.y) as usize);
                match route {
                    FleetClick::OpenAgentsPane => app.run_command(SlashCommand::Agents),
                    FleetClick::Select(i) => app.ui.fleet.set_selected(Some(i)),
                    FleetClick::Drill(sid) => {
                        app.enter_teammate_view_for_sid(&sid, true);
                    }
                }
                return;
            }
            let rect = app.ui.frame.layout.transcript_rect.get();
            if in_rect(rect, m.column, m.row) {
                // Byte-window browsing does not share transcript selection
                // coordinates, so clicks do not start a drag.
                if !app.window_browsing() {
                    let intent = TranscriptRegion {
                        view: app.region_view(),
                    }
                    .handle_down(m.column, m.row);
                    app.apply_region_intent(intent);
                }
            } else {
                app.ui.selection.transcript.clear();
                app.ui.selection.pane.clear();
                app.ui.selection.status.clear();
                app.ui.selection.approval.clear();
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.ui.selection.pane.is_dragging {
                let intent = PaneRegion {
                    view: app.region_view(),
                }
                .handle_drag(m.column, m.row);
                app.apply_region_intent(intent);
            } else if app.ui.selection.status.is_dragging {
                let intent = StatusRegion {
                    view: app.region_view(),
                }
                .handle_drag(m.column, m.row);
                app.apply_region_intent(intent);
            } else if app.ui.selection.approval.is_dragging {
                let intent = ApprovalSelection {
                    view: app.region_view(),
                }
                .handle_drag(m.column, m.row);
                app.apply_region_intent(intent);
            } else if app.ui.selection.transcript.is_dragging {
                // Edge auto-scroll first: the drag body maps the focus
                // through the post-scroll offset, so the row it lands on is
                // the one the next render shows under the cursor.
                let rect = app.ui.frame.layout.transcript_rect.get();
                app.apply_region_intent(edge_scroll_intent(rect, m.row));
                let intent = TranscriptRegion {
                    view: app.region_view(),
                }
                .handle_drag(m.column, m.row);
                app.apply_region_intent(intent);
            }
        }
        MouseEventKind::Moved => {
            // Motion without a pressed button closes any drag whose release
            // was lost. Multiple surfaces may still carry stale drag state.
            if app.ui.selection.pane.is_dragging {
                let intent = PaneRegion {
                    view: app.region_view(),
                }
                .handle_moved();
                app.apply_region_intent(intent);
            }
            if app.ui.selection.status.is_dragging {
                let intent = StatusRegion {
                    view: app.region_view(),
                }
                .handle_moved();
                app.apply_region_intent(intent);
            }
            if app.ui.selection.approval.is_dragging {
                let intent = ApprovalSelection {
                    view: app.region_view(),
                }
                .handle_moved();
                app.apply_region_intent(intent);
            }
            if app.ui.selection.transcript.is_dragging {
                let intent = TranscriptRegion {
                    view: app.region_view(),
                }
                .handle_moved();
                app.apply_region_intent(intent);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if app.ui.selection.pane.is_dragging {
                let intent = PaneRegion {
                    view: app.region_view(),
                }
                .handle_up();
                app.apply_region_intent(intent);
                return;
            }
            if app.ui.selection.status.is_dragging {
                let intent = StatusRegion {
                    view: app.region_view(),
                }
                .handle_up();
                app.apply_region_intent(intent);
                return;
            }
            if app.ui.selection.approval.is_dragging {
                let intent = ApprovalSelection {
                    view: app.region_view(),
                }
                .handle_up();
                app.apply_region_intent(intent);
                return;
            }
            if app.ui.selection.transcript.is_dragging {
                let intent = TranscriptRegion {
                    view: app.region_view(),
                }
                .handle_up();
                app.apply_region_intent(intent);
            }
        }
        MouseEventKind::ScrollUp => {
            clear_stale_click(&mut app.ui.selection.transcript);
            clear_stale_click(&mut app.ui.selection.pane);
            if app.window_browsing() {
                app.window_line_up(3);
            } else {
                app.scroll_transcript_line_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            clear_stale_click(&mut app.ui.selection.transcript);
            clear_stale_click(&mut app.ui.selection.pane);
            if app.window_browsing() {
                app.window_line_down(3);
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
    let scroll_top = app.active_scroll().top_offset();
    {
        let rows = app.ui.frame.layout.last_transcript_rows.borrow();
        paint_overlay(
            buf,
            app.ui.frame.layout.transcript_rect.get(),
            &rows,
            &app.ui.selection.transcript,
            scroll_top,
        );
    }
    {
        let rows = app.ui.frame.layout.last_pane_rows.borrow();
        paint_overlay(
            buf,
            app.ui.frame.layout.pane_rect.get(),
            &rows,
            &app.ui.selection.pane,
            0,
        );
    }
    {
        let rows = app.ui.frame.layout.last_status_rows.borrow();
        paint_overlay(
            buf,
            app.ui.frame.layout.status_rect.get(),
            &rows,
            &app.ui.selection.status,
            0,
        );
    }
    {
        let rows = app.ui.frame.layout.last_approval_rows.borrow();
        paint_overlay(
            buf,
            app.ui.frame.layout.approval_rect.get(),
            &rows,
            &app.ui.selection.approval,
            0,
        );
    }
}

impl App {
    /// The pieces a selection leaf may read: the four selections, the last
    /// published frame, the transcript's current top display row, and the
    /// clipboard port. Handing a leaf this instead of the App is what keeps a
    /// gesture from reaching domain state; the effects it asks for come back
    /// as a RegionIntent. The top is the live offset, so a gesture that
    /// scrolled first maps through the value it just moved.
    pub(crate) fn region_view(&mut self) -> RegionView<'_> {
        let scroll_top = self.active_scroll().top_offset();
        RegionView {
            selections: &mut self.ui.selection,
            frame: &self.ui.frame,
            scroll_top,
            clipboard: self.ui.clipboard.as_ref(),
        }
    }

    /// Perform what a gesture asked for. The leaf decided which row it acted
    /// on from the published frame; this is where the domain state changes
    /// and where a copy raises its notice.
    pub(crate) fn apply_region_intent(&mut self, intent: RegionIntent) {
        match intent {
            RegionIntent::None => {}
            RegionIntent::ToggleFold(ri) => {
                self.toggle_fold_at_row(ri);
                self.clear_transcript_click();
            }
            RegionIntent::ToggleSubagent(ri) => {
                self.toggle_subagent_expand_at_row(ri);
                self.clear_transcript_click();
            }
            // A row publishes a turn handle exactly when it offers a toggle,
            // but the toggle can still decline (the block is not there any
            // more). It then falls through to the collapse, which falls
            // through to the release, the way one click always did.
            RegionIntent::ToggleThinking(ri) => {
                if self.toggle_thinking_expand_at_row(ri) {
                    self.clear_transcript_click();
                } else {
                    self.apply_region_intent(RegionIntent::CollapseUnderAnchor);
                }
            }
            RegionIntent::CollapseUnderAnchor => {
                if self.collapse_expanded_under_anchor() {
                    self.ui.selection.transcript.last_click = None;
                } else {
                    let intent = TranscriptRegion {
                        view: self.region_view(),
                    }
                    .release();
                    self.apply_region_intent(intent);
                }
            }
            RegionIntent::ScrollUp => self.scroll_transcript_line_up(1),
            RegionIntent::ScrollDown => self.scroll_transcript_line_down(1),
            RegionIntent::Copied(text) => {
                self.ui
                    .notifications
                    .add(copy_toast(&text, get_clipboard_path()));
            }
        }
    }

    /// A toggle consumed the click: it must not also read as a fresh click on
    /// the next press.
    fn clear_transcript_click(&mut self) {
        self.ui.selection.transcript.last_click = None;
        self.ui.selection.transcript.clear();
    }
}

/// Dispatch a key to the right handler based on screen and overlays.
pub(crate) fn handle_key(app: &mut App, k: KeyEvent) {
    if app.pending_trust().is_some() {
        keys::handle_trust(app, k);
        return;
    }
    // ctrl+C copies a selection (with a toast); interrupts a running turn;
    // otherwise no-op. It never quits -- the panic key is not an exit.
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        if app.ui.selection.transcript.has_selection() {
            let intent = TranscriptRegion {
                view: app.region_view(),
            }
            .copy_current();
            app.apply_region_intent(intent);
            return;
        }
        if app.agent_busy() {
            app.abort_run();
        }
        return;
    }
    // A second ctrl+D while its short confirmation is visible exits from the
    // base working surface.
    if k.modifiers.contains(KeyModifiers::CONTROL)
        && k.code == KeyCode::Char('d')
        && app.ui.navigation.pane == Pane::Transcript
        && app.ui.navigation.viewport == ViewportMode::Working
    {
        let pending = app
            .ui
            .notifications
            .current()
            .is_some_and(|n| n.key == "exit-confirm");
        if pending {
            app.ui.quit = true;
        } else {
            app.ui.notifications.add(Notification::immediate(
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
    if app.ui.navigation.pane == Pane::Transcript
        && app.ui.navigation.viewport == ViewportMode::Working
        && app.ui.input.editor.is_empty()
        && app.ui.teammate_view.is_none()
    {
        let entries = &app.sessions.current.capabilities.fleet.entries;
        let running = entries.iter().filter(|e| e.completed.is_none()).count();
        if k.code == KeyCode::Char('K') && running > 0 {
            let pending = app
                .ui
                .notifications
                .current()
                .is_some_and(|n| n.key == "kill-agents-confirm");
            if pending {
                app.ui.notifications.remove("kill-agents-confirm");
                if let Err(e) = app.enqueue(ClientCommand::KillAllChildren) {
                    app.system_line(App::enqueue_failure_line("kill", e));
                }
            } else {
                app.ui.notifications.add(Notification::immediate(
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
            && let Some(i) = app.ui.fleet.selected()
            && let Some(e) = entries.get(i).filter(|e| e.completed.is_none())
        {
            if let Err(e) = app.enqueue(ClientCommand::KillChild {
                child_sid: e.agent_id.clone(),
            }) {
                app.system_line(App::enqueue_failure_line("kill", e));
            }
            return;
        }
        // No valid running selection: fall through so 'k' types.
    }
    match app.ui.navigation.screen {
        Screen::Login => keys::handle_login(app, k),
        Screen::Console => keys::handle_console(app, k),
        Screen::Working => keys::handle_working(app, k),
    }
}

#[cfg(test)]
mod tests;
