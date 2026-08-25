//! Phase-adaptive working surface. The viewport tracks the user's cognitive
//! mode, not a static frame. Three modes:
//! - Working (idle/design/done): content area, an inline palette/search cell
//!   when open, a 1-line status bar, and the input box pinned to the bottom.
//!   The progress bar lives in the status bar; there is no separate header or
//!   pane-tab strip.
//! - Focus (implement/verify): the progress bar fuses into the pane border
//!   title, input hides, the code area goes full-width with a 1-line
//!   actionable status bar.
//! - Scroll (PgUp): everything hides except a 1-line overlay status so the
//!   transcript reads full-screen. Esc/End/typing returns to the prior mode.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Color,
};

use crate::state::{App, Pane, ViewportMode};
use crate::view::{
    artifact, capability, hooks_pane, input_bar, memory_pane, model_pane, palette, queue_overlay,
    resume_picker, status, trajectory_pane, worktree_pane,
};

mod flat_transcript;
mod live_rows;
mod subagent_render;
mod working_transcript;

/// Render the working surface, dispatching on the viewport mode so the chrome
/// budget tracks the user's cognitive mode. The slash palette and inline
/// search bar render inline (a layout cell below the input box) when open in
/// Working mode, so they push the transcript up rather than float over it.
pub fn draw(f: &mut Frame, app: &App) {
    match app.viewport {
        ViewportMode::Working => draw_working(f, app),
        ViewportMode::Focus => draw_focus(f, app),
        ViewportMode::Scroll => draw_scroll(f, app),
    }
}

/// Working mode layout: content area, an inline palette/search cell when open,
/// the 1-line status bar, and the input box pinned to the bottom. No progress
/// header or pane-tab strip; the status bar carries the progress bar.
fn draw_working(f: &mut Frame, app: &App) {
    app.queue_rect.set(Rect::new(0, 0, 0, 0));
    app.pane_rect.set(Rect::new(0, 0, 0, 0));
    app.last_terminal_rows.set(f.area().height);
    // Wrap columns for the input content: terminal width minus the top/bottom
    // border (2) and the "❯ " prompt (2). Stored on App so the key handlers
    // can move the cursor in wrapped space with the same column count.
    let content_cols = (f.area().width as usize).saturating_sub(4);
    app.last_cols.set(content_cols);
    // When a browsing pane is open, the input box is hidden — its space goes
    // to the pane (no gap). input_h = 0 means the constraint reserves nothing
    // and the transcript/pane area expands.
    let pane_hides_input = matches!(
        app.pane,
        Pane::Model
            | Pane::Hooks
            | Pane::Status
            | Pane::Memory
            | Pane::Worktree
            | Pane::Trajectory
            | Pane::Resume
    );
    let input_h = if pane_hides_input {
        0
    } else {
        app.input.input_height(f.area().height, content_cols)
    };
    let total_h = f.area().height;
    // Layout top-to-bottom: transcript (fills the remainder), an optional
    // palette/search cell, the input box, an optional queued-input strip
    // (bounded, only while items are pending — moved out of the transcript
    // so a long queue never eats the interaction view), and the dim status
    // row at the bottom.
    let queue_h = if app.queue_view_open {
        0
    } else {
        queue_overlay::strip_height(app, total_h, input_h)
    };
    let mut constraints: Vec<Constraint> = vec![Constraint::Min(1)];
    // When an approval is pending, reserve a dedicated region for the card
    // below the transcript so it never paints over (Clears) transcript
    // content. The card stays pinned at the tail until decided (an approval
    // stays visible while pending), and the transcript
    // shrinks by card_h so its own tail rows are not erased.
    let approval_h = if app.approval.is_some() { 13u16 } else { 0u16 };
    // AskUserQuestion card height varies by view (question vs submit) and
    // by whether the nav bar and multi-select Submit button are shown.
    let ask_h = if let Some(aq) = app.ask_question.as_ref() {
        aq.card_height()
    } else {
        0u16
    };
    let approval_idx = if approval_h > 0 {
        constraints.push(Constraint::Length(approval_h));
        Some(constraints.len() - 1)
    } else {
        None
    };
    let ask_idx = if ask_h > 0 {
        constraints.push(Constraint::Length(ask_h));
        Some(constraints.len() - 1)
    } else {
        None
    };
    let mut overlay_idx: Option<usize> = None;
    if app.palette.open {
        constraints.push(Constraint::Length(10));
        overlay_idx = Some(constraints.len() - 1);
    }
    constraints.push(Constraint::Length(input_h));
    let input_idx = constraints.len() - 1;
    let queue_idx = if queue_h > 0 {
        constraints.push(Constraint::Length(queue_h));
        Some(constraints.len() - 1)
    } else {
        None
    };
    // The status bar hides alongside the input bar when a command pane is
    // open: the pane owns the bottom of the screen, and the status row's
    // model/dir context is not actionable while a pane is focused. Hiding
    // it reclaims the row for the pane content.
    let status_h = if pane_hides_input { 0 } else { 1 };
    constraints.push(Constraint::Length(status_h));
    let status_idx = constraints.len() - 1;
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());
    draw_main(f, outer[0], app);
    if let Some(i) = approval_idx {
        super::approval::draw(f, app, outer[i]);
    }
    if let Some(i) = ask_idx {
        super::ask_question::draw(f, app, outer[i]);
    }
    // The overlay covers the transcript while open; the footer strip is hidden
    // (queue_h=0). Skip when the queue drained so no stale empty list flashes.
    // Bottom-align the overlay so it sits near the input box (not at the top of
    // the transcript which is the screen top in Working mode — that collided with
    // the terminal chrome / flow-light border).
    if app.queue_view_open && !app.pending.is_empty() {
        let n = app.pending.len() as u16;
        let overlay_h = n + 4; // header + blank + items + blank + footer
        let y = outer[0].bottom().saturating_sub(overlay_h);
        let overlay_area = Rect {
            x: outer[0].x,
            y,
            width: outer[0].width,
            height: overlay_h,
        };
        queue_overlay::draw_queue_overlay(f, overlay_area, app);
    }
    // The resume picker now renders as a Pane (draw_command_pane) routed in
    // draw_main when app.pane == Pane::Resume, not as a floating popover. The
    // shared template keeps the transcript tail visible above the list.
    if let Some(i) = overlay_idx {
        if app.palette.open {
            palette::draw(f, app, outer[i]);
        } else {
            input_bar::draw_inline_search(f, outer[i], app);
        }
    }
    if !pane_hides_input {
        input_bar::draw_input(f, outer[input_idx], app);
    }
    if let Some(i) = queue_idx {
        queue_overlay::draw_strip(f, outer[i], app);
    }
    if status_h > 0 {
        status::draw_status_bar(f, outer[status_idx], app);
    }
}

/// Focus mode layout: the active pane renders full-width with a 1-line status
/// bar. The pane border title fuses the progress bar (see capability::
/// draw_diff_full and focus_titled_block). The status bar carries the
/// actionable keys (a/r/i) plus the progress bar.
fn draw_focus(f: &mut Frame, app: &App) {
    app.queue_rect.set(Rect::new(0, 0, 0, 0));
    app.pane_rect.set(Rect::new(0, 0, 0, 0));
    app.last_terminal_rows.set(f.area().height);
    let total_h = f.area().height;
    let queue_h = queue_overlay::strip_height(app, total_h, 0);
    let mut constraints: Vec<Constraint> = vec![Constraint::Min(1)];
    let queue_idx = if queue_h > 0 {
        constraints.push(Constraint::Length(queue_h));
        Some(constraints.len() - 1)
    } else {
        None
    };
    constraints.push(Constraint::Length(1));
    let status_idx = constraints.len() - 1;
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());
    draw_focus_main(f, outer[0], app);
    if let Some(i) = queue_idx {
        queue_overlay::draw_strip(f, outer[i], app);
    }
    status::draw_focus_status(f, outer[status_idx], app);
}

/// Scroll mode layout: the transcript fills the screen for full-history
/// reading, with a 1-line overlay status bar showing the line position plus
/// search/tail hints.
fn draw_scroll(f: &mut Frame, app: &App) {
    app.queue_rect.set(Rect::new(0, 0, 0, 0));
    app.pane_rect.set(Rect::new(0, 0, 0, 0));
    app.last_terminal_rows.set(f.area().height);
    let total_h = f.area().height;
    let queue_h = queue_overlay::strip_height(app, total_h, 0);
    let mut constraints: Vec<Constraint> = vec![Constraint::Min(1)];
    let queue_idx = if queue_h > 0 {
        constraints.push(Constraint::Length(queue_h));
        Some(constraints.len() - 1)
    } else {
        None
    };
    constraints.push(Constraint::Length(1));
    let status_idx = constraints.len() - 1;
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());
    // Byte-window mode renders flat (skips the fold slot layer -- one screen
    // at a time has no whole-vec fold object) through its own path that does
    // not touch TranscriptScroll/display_slots/total. Under threshold, the
    // whole-log snapshot renders through the standard slot-based path.
    if app.window_mode {
        flat_transcript::draw_flat_transcript(f, outer[0], app);
    } else {
        working_transcript::draw_transcript(f, outer[0], app);
    }
    if let Some(i) = queue_idx {
        queue_overlay::draw_strip(f, outer[i], app);
    }
    status::draw_scroll_status(f, outer[status_idx], app);
}

/// Render the active pane full-width in Focus mode (a full-width code area).
fn draw_focus_main(f: &mut Frame, area: Rect, app: &App) {
    match app.pane {
        Pane::Diff => capability::draw_diff_full(f, area, app),
        Pane::Transcript => working_transcript::draw_transcript(f, area, app),
        Pane::Memory => draw_memory_pane(f, area, app),
        Pane::Worktree => draw_worktree_pane(f, area, app),
        Pane::Trajectory => draw_trajectory_pane(f, area, app),
        Pane::Status => draw_status_pane(f, area, app),
        Pane::Resume => draw_resume_pane(f, area, app),
        Pane::Hooks => draw_hooks_pane(f, area, app),
        Pane::Model => draw_model_pane(f, area, app),
        _ => capability::draw(f, area, app),
    }
}

/// Render the main area (Working mode). When a runner is wired (agent-chat
/// mode), the main area is always the transcript full-width: the working
/// screen is a chat stream, not a stage-driven capability split. Editing
/// happens through the agent's tools (Write/Read/Bash via approval), not
/// through the spec/diff/review panes. The artifact pane is the one exception:
/// /artifact opens it for advanced inline review. Without a runner (stub
/// mode), the legacy full-width diff / side-by-side split is kept so existing
/// tests render unchanged.
fn draw_main(f: &mut Frame, area: Rect, app: &App) {
    // The permission rule manager renders inline (the Pane primitive: a
    // ─ Divider + a cleared region below), not as a full-screen overlay or
    // a rounded card. The transcript tail occupies the main area above it;
    // the Pane owns a lower band. One path for both stub and wired modes —
    // no stub-vs-wired style divergence (/permissions renders
    // below the REPL prompt in both contexts).
    if matches!(app.pane, Pane::Permission) {
        draw_permission_pane(f, area, app);
        return;
    }
    // The /memory pane renders inline (the Pane primitive, shared with
    // /permissions and /search): transcript tail above, the memory list +
    // toggle rows in the lower band. NOT an external editor — the user stays
    // in-TUI, mirroring /permissions and /search. One path for stub + wired.
    if matches!(app.pane, Pane::Memory) {
        draw_memory_pane(f, area, app);
        return;
    }
    // The /worktrees pane renders inline (the Pane primitive, shared with
    // /permissions /search /memory): transcript tail above, the worktree list
    // in the lower band. The worktree-list surface has no
    // equivalent command elsewhere, so this pane is the in-TUI manager. Selection +
    // copy come free via the shared PaneSurface (the mouse router keys on
    // the pane rect, which draw_command_pane stashes).
    if matches!(app.pane, Pane::Worktree) {
        draw_worktree_pane(f, area, app);
        return;
    }
    if matches!(app.pane, Pane::Trajectory) {
        draw_trajectory_pane(f, area, app);
        return;
    }
    // The /status pane renders inline (the Pane primitive, shared with
    // /permissions /memory /worktrees /resume): transcript tail above, the
    // status fields below. NOT a transcript text dump — the command opens a
    // live pane so Esc dismisses + the field set stays visible while the
    // cache refreshes.
    if matches!(app.pane, Pane::Status) {
        draw_status_pane(f, area, app);
        return;
    }
    // The /resume picker renders inline (the Pane primitive): transcript tail
    // above, the filtered session list below. NOT a bottom-anchored popover;
    // the shared template keeps the transcript tail visible + selection +
    // copy working for free.
    if matches!(app.pane, Pane::Resume) {
        draw_resume_pane(f, area, app);
        return;
    }
    // The /hooks pane renders inline (the Pane primitive): transcript tail
    // above, the hook list below.
    if matches!(app.pane, Pane::Hooks) {
        draw_hooks_pane(f, area, app);
        return;
    }
    // The /model pane renders inline (the Pane primitive): transcript tail
    // above, the selectable model list below.
    if matches!(app.pane, Pane::Model) {
        draw_model_pane(f, area, app);
        return;
    }
    if app.session.is_some() {
        // Full-screen overlays that take over the main area in agent-chat
        // mode (otherwise the main area is the chat transcript). The artifact
        // pane is the slash-command surface that owns the whole main area when
        // open.
        if matches!(app.pane, Pane::Artifact) {
            artifact::draw(f, area, app);
            return;
        }
        working_transcript::draw_transcript(f, area, app);
        return;
    }
    if matches!(app.pane, Pane::Diff) {
        capability::draw_diff_full(f, area, app);
        return;
    }
    if matches!(app.pane, Pane::Transcript) {
        working_transcript::draw_transcript(f, area, app);
        return;
    }
    if matches!(app.pane, Pane::Artifact) {
        artifact::draw(f, area, app);
        return;
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    working_transcript::draw_transcript(f, cols[0], app);
    capability::draw(f, cols[1], app);
}

/// Render a slash-command pane: transcript tail in the upper band, the Pane
/// frame (─ divider + cleared region) in the lower band with the command's
/// content closure. The shared shape for /permissions, /search, and future
/// slash-command panes — one template, not a copy per command. Capped at half
/// the main area so the transcript stays visible.
fn draw_command_pane<C>(f: &mut Frame, area: Rect, app: &App, pane_h: u16, content: C)
where
    C: Fn(&mut Frame, Rect, &App),
{
    let pane_h = pane_h.min(area.height / 2).max(8);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(pane_h)])
        .split(area);
    working_transcript::draw_transcript(f, chunks[0], app);
    crate::view::pane::render(f, chunks[1], Color::Cyan, |f, inner| {
        content(f, inner, app);
        stash_pane_rows(f, inner, app);
    });
}

/// Stash the rendered pane content rows and inner rect so the in-app
/// selection + copy path can reach text the slash-command pane drew. The
/// panes (/permissions /search /memory) render through arbitrary widgets
/// (List, Paragraph, SearchBox); rather than duplicate each widget's row
/// construction, the rendered cells are read straight from the frame buffer
/// after the content closure draws — the buffer is the single source of
/// truth for what the user sees. Trailing blanks are trimmed per row so a
/// drag to the right edge past text does not pad the clipboard. The content
/// closure has already finished drawing by the time this runs, so the cells
/// reflect the final pane content for the frame.
fn stash_pane_rows(f: &mut Frame, inner: Rect, app: &App) {
    app.pane_rect.set(inner);
    let mut rows = app.last_pane_rows.borrow_mut();
    rows.clear();
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let buf = f.buffer_mut();
    for ry in 0..inner.height {
        let y = inner.y + ry;
        let mut row = String::with_capacity(inner.width as usize);
        for rx in 0..inner.width {
            let x = inner.x + rx;
            if let Some(cell) = buf.cell((x, y)) {
                row.push_str(cell.symbol());
            }
        }
        rows.push((crate::selection::TAG_PLAIN, row.trim_end().to_string()));
    }
}

/// Render the /permissions Pane: transcript tail above, the rule manager in the
/// lower band. Capped at half the main area so the transcript stays visible.
fn draw_permission_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        PERMISSION_PANE_HEIGHT,
        capability::draw_permission_content,
    );
}

/// Default height /permissions asks for: tabs + description + SearchBox + an
/// 8-row list + footer. Capped at half the main area by the caller.
const PERMISSION_PANE_HEIGHT: u16 = 18;

/// Render the /memory pane: transcript tail above, the stored-memory list plus
/// the auto-memory / auto-dream toggle rows in the lower band. Shares the
/// draw_command_pane template with /permissions and /search so the three
/// slash-command panes have one shape. The pane stays in-TUI (no external
/// editor): the user browses, shows a body, and flips toggles without leaving
/// the conversation.
fn draw_memory_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(f, area, app, MEMORY_PANE_HEIGHT, memory_pane::draw_content);
}

/// Default height /memory asks for: a header + an 8-row list + 2 toggle rows
/// + a footer hint. Capped at half the main area by the caller.
const MEMORY_PANE_HEIGHT: u16 = 16;

/// Render the /worktrees pane: transcript tail above, the linked-worktree
/// list in the lower band. Shares the draw_command_pane template with
/// /permissions /search /memory so the slash-command panes have one shape.
/// The pane is the in-TUI worktree manager: browse, Enter to enter one, d to
/// remove one (the remove action routes through the agent worktree tool so
/// the approval gate still fires).
fn draw_worktree_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        WORKTREE_PANE_HEIGHT,
        worktree_pane::draw_content,
    );
}

/// Render the /trajectory pane. Level 0 (turn list) shares the
/// draw_command_pane template so the transcript tail stays visible above while
/// the user scans the session timeline. Drilling into a turn (Level 1) or an
/// event (Level 2) goes fullscreen: the user is now focused on one span, the
/// transcript tail is not useful context, and a turn's event list plus its
/// header and key-hint footer need the full height to stay visible. Follows
/// the /search detail fullscreen pattern. Shows mock trajectory data until the
/// observability log is wired into the agent loop.
fn draw_trajectory_pane(f: &mut Frame, area: Rect, app: &App) {
    if app.trajectory_level.get() >= 1 {
        crate::view::pane::render(f, area, Color::Cyan, |f, inner| {
            trajectory_pane::draw_content(f, inner, app);
            stash_pane_rows(f, inner, app);
        });
    } else {
        draw_command_pane(
            f,
            area,
            app,
            TRAJECTORY_PANE_HEIGHT,
            trajectory_pane::draw_content,
        );
    }
}

const TRAJECTORY_PANE_HEIGHT: u16 = 20;

/// Render the /status pane: transcript tail above, the status fields
/// (identity + runtime + session + tasks) in the lower band. Shares the
/// draw_command_pane template with /permissions /memory /worktrees /resume.
/// A live pane (not a transcript dump): Esc dismisses, and the field set
/// stays visible while the cache refreshes.
fn draw_status_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        status::pane::STATUS_PANE_HEIGHT,
        status::pane::draw_content,
    );
}

/// Render the /resume picker pane: transcript tail above, the filtered
/// session list in the lower band. Shares the draw_command_pane template
/// with /permissions /memory /worktrees /status. The picker state machine
/// (SessionPickerState) + keys (Up/Down/Enter/Esc/char) stay; only the
/// container moves from a bottom-anchored popover to the shared template.
fn draw_resume_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        RESUME_PANE_HEIGHT,
        resume_picker::draw_content,
    );
}

/// Default height /resume asks for: a header + up to 10 visible rows + a
/// footer. Capped at half the main area by draw_command_pane.
const RESUME_PANE_HEIGHT: u16 = 16;

/// Render the /hooks pane: transcript tail above, the hook list below.
fn draw_hooks_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        hooks_pane::HOOKS_PANE_HEIGHT,
        hooks_pane::draw_content,
    );
}

/// Render the /model pane: transcript tail above, the selectable model list in
/// the lower band. Shares the draw_command_pane template with the other
/// slash-command panes. Up/Down navigate, Enter sets the default, s uses this
/// session only, Esc closes.
fn draw_model_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        model_pane::MODEL_PANE_HEIGHT,
        model_pane::draw_content,
    );
}

/// Default height /worktrees asks for: a header + an 8-row list + a footer
/// hint. Capped at half the main area by the caller.
const WORKTREE_PANE_HEIGHT: u16 = 14;

#[cfg(test)]
#[path = "working_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "working_bash_progress_tests.rs"]
mod working_bash_progress_tests;
#[cfg(test)]
#[path = "working_highlight_tests.rs"]
mod working_highlight_tests;
