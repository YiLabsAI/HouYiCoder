//! Terminal lifecycle, event loop, and top-level input routing.
//! Screen-specific behavior remains in the view and key modules.

pub mod state;

mod event_loop;
pub(crate) mod frame_clock;
pub(crate) mod wake;

/// The synchronized-frame markers the frame writer brackets a draw with.
pub(crate) use houyicoder_platform::desktop::terminal::SYNC_BEGIN;
pub(crate) use houyicoder_platform::desktop::terminal::SYNC_END;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{Frame, Terminal, layout::Rect};
use ratatui_crossterm::CrosstermBackend;
use std::cmp;
use std::error::Error;
use std::io::{self, BufWriter, stdout};
use std::time::{Duration, Instant};

use houyicoder_protocol::frontend::SlashCommand;

use crate::agent_message::ClientCommand;
use crate::app::event_loop::{BracketedWriter, FRAME_BUFFER_BYTES, TerminalWriter};
use crate::app::wake::ui_wake;
use crate::composition::{FrontendStartup, RunnerBundle, build_app};
use crate::input::terminal_events::EventReader;
use crate::keys;
use crate::keys::filter::KeyFilter;
use crate::keys::pane_predicates::child_action_pending;
use crate::notifications::{NotifKind, Notification, copy_toast};
use crate::selection::get_clipboard_path;
use crate::selection::region::{
    ApprovalSelection, PaneRegion, Region, RegionIntent, RegionView, StatusRegion,
    TranscriptRegion, clear_stale_click, edge_scroll_intent, paint_overlay,
};
use crate::state::{App, Pane, Screen, ViewportMode};
use crate::terminal_guard::{CrosstermModes, TerminalGuard, assert_mouse_modes};
use crate::termination::{TerminationWatch, watch_termination};
use crate::ui::selection::paint_regions;
use crate::view::working::fleet_strip::{FleetClick, click_route};

/// Run the alternate-screen TUI and restore terminal state on exit.
/// Mouse input belongs to in-app scrolling and selection while active.
type ResumeBuilder = Box<dyn Fn(&str) -> Result<(RunnerBundle, FrontendStartup), Box<dyn Error>>>;

pub fn run_with_runner(
    bundle: RunnerBundle,
    startup: FrontendStartup,
    resume_builder: Option<ResumeBuilder>,
) -> io::Result<Option<String>> {
    // The guard owns every terminal mode change below, so an early return or
    // a panic unwinding past this loop restores the console. The watch is
    // installed before the guard so a termination signal between taking the
    // modes and hearing a signal still reaches the loop's teardown path.
    let termination = {
        let wake = ui_wake();
        watch_termination(move || wake.wake())?
    };
    let mut guard = TerminalGuard::acquire(CrosstermModes::default())?;
    // One write per frame: the buffer is flushed by the draw, and the cursor
    // escape that follows it is ordered after the frame bytes.
    let backend = CrosstermBackend::new(BufWriter::with_capacity(FRAME_BUFFER_BYTES, stdout()));
    let mut terminal = Terminal::new(backend)?;
    let mut app = build_app(bundle, startup);
    app.startup_handshake(Duration::from_secs(2));
    {
        let mut writer = TerminalWriter::new(&mut terminal);
        let mut bracketed = BracketedWriter::new(&mut writer);
        event_loop::run(
            &mut app,
            EventReader::shared(),
            ui_wake(),
            &termination,
            &mut bracketed,
            resume_builder.as_deref(),
        )?;
    }

    // Restore the terminal modes the guard owns. The tab title and the
    // progress indicator are restored inside the guard's teardown, so this is
    // the one teardown step the loop has to drive explicitly.
    tracing::info!(
        quit = app.ui.quit,
        signal = termination.requested(),
        "TUI run loop exiting"
    );
    guard.release();
    Ok(app.exit_target(&termination))
}

impl App {
    /// What the loop hands up when it ends. A signal ends the process, not
    /// the session: the run in flight is cancelled and a queued resume target
    /// is dropped. A normal quit hands up whatever the queue holds.
    fn exit_target(&mut self, termination: &TerminationWatch) -> Option<String> {
        if termination.requested() {
            self.stop_for_signal();
            return None;
        }
        self.ui.input.queue.drain_resume_target()
    }
}

/// Apply one terminal event and report whether the view changed.
pub(crate) fn handle_event(
    app: &mut App,
    event: Event,
    filter: &mut KeyFilter,
) -> io::Result<bool> {
    match event {
        Event::Key(k) => {
            // A release, a held action key, and the second press of one
            // non-BMP character change nothing and need no redraw.
            if !filter.admit(app, &k, Instant::now()) {
                return Ok(false);
            }
            // Text pasted just before this key belongs before it. The run loop
            // waits out the paste gap before routing, so a key pressed within
            // that gap must route the buffered text first or it would act on a
            // draft without it (an Enter would submit an empty box).
            app.flush_pending_paste_now();
            handle_key(app, k);
            Ok(true)
        }
        Event::Mouse(m) => {
            handle_mouse(app, m);
            Ok(true)
        }
        Event::Paste(data) => {
            // A card that owns the keyboard owns the paste too: the composer
            // is not receiving keys while it is up, so text pasted into its
            // Other row would otherwise land in a draft nobody can see.
            if let Some(aq) = app.ask_question_mut()
                && aq.cursor_on_other()
            {
                aq.append_other(&data);
                return Ok(true);
            }
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
            if let Err(e) = assert_mouse_modes() {
                tracing::warn!(error = %e, "mouse mode assertion on resize failed");
            }
            Ok(true)
        }
        Event::FocusGained => {
            if let Err(e) = assert_mouse_modes() {
                tracing::warn!(error = %e, "mouse mode assertion on focus failed");
            }
            app.ui.terminal_focused = true;
            app.report_focus(true);
            Ok(true)
        }
        Event::FocusLost => {
            // Keep the logical caret position while hiding it from an
            // unfocused terminal so input composition resumes correctly.
            app.ui.terminal_focused = false;
            app.report_focus(false);
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
            // The composer owns a press inside it: the caret goes where the
            // pointer landed and a drag from there selects the draft.
            if app.press_in_composer(&m) {
                app.composer_press(&m);
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
            if app.ui.selection.input.is_dragging {
                app.composer_drag(&m);
            } else if app.ui.selection.pane.is_dragging {
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
            if app.ui.selection.input.is_dragging {
                app.composer_moved();
            }
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
            if app.ui.selection.input.is_dragging {
                app.composer_release();
                return;
            }
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

/// Paint each region's selection after layout and before the frame flush.
/// Transcript coordinates include scroll offset; pane coordinates are local.
pub(crate) fn apply_selection_overlay(f: &mut Frame, app: &App) {
    let buf = f.buffer_mut();
    let scroll_top = app.active_scroll().top_offset();
    {
        let rows = app.ui.frame.layout.last_transcript_rows.borrow();
        // The ends are named by the lines under them, so resolve them into
        // the rows this frame drew before the range is mapped to screen rows.
        let ids = app.ui.frame.layout.last_all_ids.borrow();
        let sel = app.ui.selection.transcript.resolved(&ids);
        paint_overlay(
            buf,
            app.ui.frame.layout.transcript_rect.get(),
            &rows,
            &sel,
            scroll_top,
        );
    }
    paint_regions(buf, app);
}

impl App {
    /// The pieces a selection leaf may read: the selections, the last frame,
    /// the transcript's top row, and the clipboard. Handing a leaf this
    /// instead of the App keeps a gesture off domain state; its effects come
    /// back as a RegionIntent.
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
            // but the toggle can decline. It then falls through to collapse
            // and release, the way one click always did.
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
            RegionIntent::DrillChild(child_sid) => {
                self.enter_teammate_view_for_sid(&child_sid, true);
                self.clear_transcript_click();
            }
            RegionIntent::ToggleResult(ri) => {
                let id = self.ui.frame.keys.borrow().callid_at(ri);
                if let Some(id) = id {
                    self.ui.transcript_view.toggle_result(id);
                    self.clear_transcript_click();
                }
            }
            RegionIntent::ToggleApprovalExpand => {
                if let Some(card) = self.approval_mut() {
                    card.expanded = !card.expanded;
                }
                self.ui.selection.approval.last_click = None;
                self.ui.selection.approval.clear();
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
impl App {
    /// Stop every running child, behind a two-press confirmation: the first
    /// press raises the notice, the second within its window sends the kill.
    /// Killing a whole fan-out is not undoable, so the chord is never a single
    /// keystroke.
    fn confirm_kill_all_children(&mut self, running: usize) {
        let pending = self
            .ui
            .notifications
            .current()
            .is_some_and(|n| n.key == "kill-agents-confirm");
        if pending {
            self.ui.notifications.remove("kill-agents-confirm");
            if let Err(e) = self.enqueue(ClientCommand::KillAllChildren) {
                self.system_line(App::enqueue_failure_line("kill", e));
            }
            return;
        }
        self.ui.notifications.add(Notification::immediate(
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
}

pub(crate) fn handle_key(app: &mut App, k: KeyEvent) {
    if app.pending_trust().is_some() {
        keys::handle_trust(app, k);
        return;
    }
    // ctrl+C copies a selection (with a toast); interrupts a running turn or
    // a running direct shell command; otherwise no-op. It never quits -- the
    // panic key is not an exit. Either case of the letter matches: a layout
    // can deliver the capitalized form with control held.
    if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c' | 'C')) {
        if !app.copy_active_selection() && (app.agent_busy() || app.shell_run_active()) {
            app.abort_run();
        }
        return;
    }
    // A second ctrl+D while its short confirmation is visible exits from the
    // base working surface.
    if k.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(k.code, KeyCode::Char('d' | 'D'))
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
    // its non-terminal interrupt role. Both are plain-key gestures: Ctrl+K is
    // the draft's kill-to-line-end, so it must not be read as a child kill
    // just because the draft happens to be empty.
    let plain_key = !k
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    if plain_key && child_action_pending(app) {
        let entries = &app.sessions.current.capabilities.fleet.entries;
        let running = entries.iter().filter(|e| e.outcome.is_none()).count();
        if k.code == KeyCode::Char('K') && running > 0 {
            app.confirm_kill_all_children(running);
            return;
        }
        if k.code == KeyCode::Char('k')
            && running > 0
            && let Some(i) = app.ui.fleet.selected()
            && let Some(e) = entries.get(i).filter(|e| e.outcome.is_none())
        {
            if let Err(e) = app.enqueue(ClientCommand::KillChild {
                child_sid: e.agent_id.clone(),
            }) {
                app.system_line(App::enqueue_failure_line("kill", e));
            }
            return;
        }
        // No valid running selection: fall through so 'k' types.
        if k.code == KeyCode::Char('b')
            && running > 0
            && let Some(i) = app.ui.fleet.selected()
            && let Some(e) = entries.get(i).filter(|e| e.outcome.is_none())
        {
            if let Err(e) = app.enqueue(ClientCommand::BackgroundChild {
                child_sid: e.agent_id.clone(),
            }) {
                app.system_line(App::enqueue_failure_line("background", e));
            }
            return;
        }
    }
    match app.ui.navigation.screen {
        Screen::Login => keys::handle_login(app, k),
        Screen::Console => keys::handle_console(app, k),
        Screen::Working => keys::handle_working(app, k),
    }
}

#[cfg(test)]
mod tests;
