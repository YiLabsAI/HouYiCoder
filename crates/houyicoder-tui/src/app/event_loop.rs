//! The event-driven run loop.
//!
//! One wake from any producer drains every source and draws at most one frame.
//! Frame spacing is held to the rate a terminal can usefully repaint, so a
//! paste or a token burst becomes one frame rather than hundreds.
//!
//! A quiet session still schedules a wake for each timer it owns, such as the
//! status poll or the child-completion debounce. Only a session with no live
//! timer blocks without one, and any producer ends that block.

use std::error::Error as StdError;
use std::io::{self, Error};
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::Backend;

use super::{apply_selection_overlay, handle_event};
use crate::app::frame_clock::FrameClock;
use crate::app::wake::UiWake;
use crate::composition::ResumeBuilderRef;
use crate::input::terminal_events::EventReader;
use crate::keys::filter::KeyFilter;
use crate::selection::region::edge_scroll_if_at_edge;
use crate::state::{App, Pane};
use crate::terminal_guard::set_terminal_progress;
use crate::termination::TerminationWatch;
use crate::ui::switch_state::WAKE_DEBOUNCE;
use crate::view;

/// Bytes buffered for one frame before the write reaches the terminal.
pub(crate) const FRAME_BUFFER_BYTES: usize = 64 * 1024;

/// How often a live animation advances while nothing else happens.
const ANIMATION_INTERVAL: Duration = Duration::from_millis(100);
/// How often the fleet strip's elapsed clocks advance.
const FLEET_CLOCK_INTERVAL: Duration = Duration::from_secs(1);
/// The gap that ends a paste, matching the paste accumulator's flush window.
const PASTE_FLUSH_GAP: Duration = Duration::from_millis(50);
/// How often a pointer held at the transcript edge scrolls one line.
///
/// The edge check runs on every wake, and a streaming reply or a fast mouse
/// wakes the loop hundreds of times a second; without its own cadence the
/// transcript would race away far faster than the user can read it.
const EDGE_SCROLL_INTERVAL: Duration = Duration::from_millis(100);

/// One frame of output.
pub(crate) trait FrameWriter {
    /// Draw the current state of the app.
    fn write_frame(&mut self, app: &mut App) -> io::Result<()>;
}

/// A frame writer that draws to the terminal through a frame-sized buffer.
pub(crate) struct TerminalWriter<'a, B: Backend> {
    terminal: &'a mut Terminal<B>,
}

impl<'a, B: Backend> TerminalWriter<'a, B> {
    /// A writer over the session's terminal.
    pub(crate) fn new(terminal: &'a mut Terminal<B>) -> Self {
        Self { terminal }
    }
}

/// Every backend error is reported as an io error, so the writer accepts any
/// ratatui backend instead of only those whose error already converts.
impl<B: Backend> FrameWriter for TerminalWriter<'_, B>
where
    B::Error: StdError + Send + Sync + 'static,
{
    fn write_frame(&mut self, app: &mut App) -> io::Result<()> {
        let size = self.terminal.size().map_err(Error::other)?;
        // A window that reports no cells has nothing to paint; drawing into a
        // zero-area layout only fails the backend write.
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        app.load_older_frames();
        // Stage the frame: the draw writes into a fresh value, held aside
        // until the draw reports success.
        let published = app.ui.stage_frame();
        let drawn = self
            .terminal
            .draw(|f| {
                view::draw(f, app);
                apply_selection_overlay(f, app);
            })
            .map_err(Error::other);
        // A failed draw puts the last published frame back before the error
        // unwinds to the terminal guard, so nothing reads a half-written one.
        app.ui.settle_frame(published, drawn)?;
        // The draw above reports no frame cursor, so it has already hidden the
        // cursor and flushed the frame bytes. Moving the hidden caret through
        // the same backend keeps the frame and the escape in one ordered
        // sequence instead of two writers racing for the terminal.
        if let Some((x, y)) = app.ui.frame.feedback.native_cursor_position.get()
            && let Err(e) = self.terminal.set_cursor_position((x, y))
        {
            tracing::warn!(error = %e, "cursor positioning failed");
        }
        Ok(())
    }
}

/// Run until the user quits or a termination signal arrives.
pub(crate) fn run(
    app: &mut App,
    reader: &EventReader,
    wake: &UiWake,
    termination: &TerminationWatch,
    writer: &mut impl FrameWriter,
    resume_builder: Option<&ResumeBuilderRef>,
) -> io::Result<()> {
    let mut dirty = true;
    // Tracks the last busy state reported to the terminal emulator through the
    // progress sequence, so the escape is written only on transitions.
    let mut reported_busy = false;
    let mut filter = KeyFilter::default();
    let mut clock = FrameClock::default();
    let mut last_edge_scroll: Option<Instant> = None;
    while !termination.should_exit(app.ui.quit) {
        // One wake serves every producer: drain the whole terminal burst and
        // every protocol message before deciding what to draw.
        let drained = reader.drain();
        if drained.gone {
            // Nothing left to type into and no event left to draw: leaving
            // through the teardown restores what the guard took.
            tracing::warn!("terminal input channel closed, quitting");
            app.ui.quit = true;
        }
        for event in drained.events {
            dirty |= handle_event(app, event, &mut filter)?;
        }
        if app.poll_agent() {
            dirty = true;
        }
        if app.agent_busy() != reported_busy {
            if let Err(e) = set_terminal_progress(app.agent_busy()) {
                tracing::warn!(error = %e, "terminal progress update failed");
            }
            reported_busy = app.agent_busy();
        }
        // Idle drain consumes queued work only after a clean run; interrupted
        // work stays available for editing.
        app.idle_drain(resume_builder, &mut dirty);
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
        if app.tick_auto_approve(now) {
            dirty = true;
        }
        app.tick_tasks(now);
        if app.sessions.current.todos.prune(now) {
            dirty = true;
        }
        if app.pump_background_reads() {
            dirty = true;
        }
        if last_edge_scroll
            .is_none_or(|at| now.saturating_duration_since(at) >= EDGE_SCROLL_INTERVAL)
            && edge_scroll_if_at_edge(app)
        {
            dirty = true;
            last_edge_scroll = Some(now);
        }
        if app.flush_pending_paste() {
            dirty = true;
        }
        // A key, a card answer, or a signal handled above may have ended the
        // session. Leave before the frame window: a quitting loop must not
        // sleep out the cooldown or paint a frame nobody asked for.
        if termination.should_exit(app.ui.quit) {
            break;
        }
        if dirty || app.agent_busy() || app.compacting() || app.shell_run_active() {
            let now = Instant::now();
            if let Some(left) = clock.cooldown(now) {
                wait_out_frame(wake, app, termination, left);
                continue;
            }
            writer.write_frame(app)?;
            clock.note_frame(Instant::now());
            dirty = false;
        }
        let timeout = wait_timeout(app, &clock, dirty, Instant::now());
        wake.wait(timeout);
    }
    Ok(())
}

/// Wait out the rest of a frame window, merging everything that arrives inside
/// it into the next frame.
///
/// A producer that keeps waking the loop (a token stream, a fast mouse) must
/// not turn the window into a spin: the wait repeats until the window closes,
/// and only a termination request leaves early.
fn wait_out_frame(wake: &UiWake, app: &App, termination: &TerminationWatch, left: Duration) {
    let deadline = Instant::now() + left;
    loop {
        if termination.should_exit(app.ui.quit) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        wake.wait(Some(remaining));
    }
}

/// How long the loop may block before a live timer needs the screen again.
/// None means nothing is live, so a quiet session sleeps until a producer
/// wakes it.
fn wait_timeout(app: &App, clock: &FrameClock, dirty: bool, now: Instant) -> Option<Duration> {
    if dirty {
        return Some(clock.cooldown(now).unwrap_or_default());
    }
    let mut timeout: Option<Duration> = None;
    let mut within = |wait: Duration| {
        timeout = Some(timeout.map_or(wait, |current| current.min(wait)));
    };
    if app.agent_busy() || app.compacting() || app.shell_run_active() {
        within(ANIMATION_INTERVAL);
    }
    if app.ui.notifications.current().is_some()
        || app.approval().is_some()
        || app.ask_question().is_some()
        || app.pending_trust().is_some()
    {
        within(ANIMATION_INTERVAL);
    }
    // The idle wake batches near-simultaneous child completions. Nothing else
    // is scheduled while a session sits idle, so the debounce window itself
    // has to bring the loop back.
    if let Some(started) = app.ui.notification_wake.window_started {
        within(WAKE_DEBOUNCE.saturating_sub(now.saturating_duration_since(started)));
    }
    // A finished checklist clears itself once every row has been completed
    // for the visibility window.
    if let Some(left) = app.sessions.current.todos.clear_delay(now) {
        within(left);
    }
    if app.ui.navigation.pane == Pane::Tasks {
        within(ANIMATION_INTERVAL);
    }
    if app
        .sessions
        .current
        .capabilities
        .fleet
        .entries
        .iter()
        .any(|entry| entry.outcome.is_none())
    {
        within(FLEET_CLOCK_INTERVAL);
    }
    if !app.agent_busy()
        && !app.reverse_request_in_flight()
        && app.sessions.current.connection.is_some()
    {
        // The status bar refreshes from a periodic query, so an otherwise
        // idle session still wakes once per poll interval.
        within(app.sessions.current.capabilities.status.poll_remaining());
    }
    if dragging(app) {
        within(ANIMATION_INTERVAL);
    }
    if let Some(last) = app.ui.input.pending_paste.last {
        within(PASTE_FLUSH_GAP.saturating_sub(now.saturating_duration_since(last)));
    }
    timeout
}

/// Whether a pointer drag is in progress in any region. A drag scrolls at
/// the transcript edge without further events, so it needs a timer.
fn dragging(app: &App) -> bool {
    let selection = &app.ui.selection;
    selection.transcript.is_dragging
        || selection.pane.is_dragging
        || selection.status.is_dragging
        || selection.approval.is_dragging
        || selection.input.is_dragging
}

#[cfg(test)]
mod tests;
