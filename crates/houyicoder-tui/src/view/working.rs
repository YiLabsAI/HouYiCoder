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
};

use super::approval::{
    card_height as approval_card_height, draw as approval_draw, essential_rows as approval_floor,
};
use super::teammate_view::draw_banner;
use crate::keys::pane_replaces_input;
use crate::selection::TAG_PLAIN;
use crate::state::{App, Pane, ViewportMode};
use crate::view::{
    artifact, capability, input_bar, palette, queue_overlay, skill_picker, status, toast,
};

pub(crate) mod command_state;
mod compaction_rows;
mod delegation_rows;
pub(crate) mod fleet_strip;
mod footer_budget;
pub(crate) mod history_window;
mod notice_rows;
mod panes;
mod shell_row;
mod tail_rows;
pub(crate) mod transcript;

/// Render the working surface, dispatching on the viewport mode so the chrome
/// budget tracks the user's cognitive mode. The slash palette and inline
/// search bar render inline (a layout cell below the input box) when open in
/// Working mode, so they push the transcript up rather than float over it.
pub fn draw(f: &mut Frame, app: &App) {
    match app.ui.navigation.viewport {
        ViewportMode::Working => draw_working(f, app),
        ViewportMode::Focus => draw_focus(f, app),
        ViewportMode::Scroll => draw_scroll(f, app),
    }
}

/// Whether the open command pane retracts the input box. /permissions is
/// excluded — its Add/Remove sub-mode types rule specs into the input box.
fn pane_hides_input(app: &App) -> bool {
    pane_replaces_input(app.ui.navigation.pane)
}

/// Whether the open command pane retracts the status bar. Broader than
/// pane_hides_input: /permissions keeps its input box but still hides the
/// status row. Caveat: the mode label + ask-before-git toggle live on the
/// status bar, so hiding drops two permission-relevant reads (follow-up:
/// move them into the pane).
fn pane_hides_status(app: &App) -> bool {
    pane_hides_input(app) || matches!(app.ui.navigation.pane, Pane::Permission)
}

/// Working mode layout: content area, an inline palette/search cell when open,
/// the 1-line status bar, and the input box pinned to the bottom. No progress
/// header or pane-tab strip; the status bar carries the progress bar.
fn draw_working(f: &mut Frame, app: &App) {
    super::reset_click_regions(app);
    app.ui.frame.feedback.terminal_rows.set(f.area().height);
    // Wrap columns for the input content: terminal width minus the top/bottom
    // border (2) and the "❯ " prompt (2). Fed back with the rest of the
    // layout feedback so the key handlers move the cursor with the same
    // column count the render itself used.
    let content_cols = (f.area().width as usize).saturating_sub(4);
    app.ui.frame.feedback.terminal_cols.set(content_cols);
    let pane_hides_input = pane_hides_input(app);
    let input_h = if pane_hides_input {
        0
    } else {
        app.ui
            .input
            .editor
            .input_height(f.area().height, content_cols)
    };
    let total_h = f.area().height;
    // Layout top-to-bottom: transcript (fills the remainder), an optional
    // palette/search cell, the queued-input strip (bounded, only while items
    // are pending — sits above the input box so it reads as context for what
    // the user is about to type, not as a footer afterthought), the input
    // box, and the dim status row at the bottom.
    // Both pinned strips draw from one budget, in priority order, so their
    // sum cannot starve the transcript.
    let (queue_want, fleet_want) = if pane_hides_input {
        (0, 0)
    } else {
        (queue_overlay::strip_want(app), fleet_strip::want(app))
    };
    let footer = footer_budget::allocate(total_h, input_h, queue_want, fleet_want);
    app.ui.frame.layout.fleet_granted.set(footer.fleet);
    let layout = build_working_layout(app, f.area(), input_h, footer.queue, footer.fleet);
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints(layout.constraints)
        .split(f.area());
    if let Some(i) = layout.slots.banner {
        draw_banner(f, app, outer[i]);
    }
    draw_main(f, outer[layout.transcript_idx], app);
    if !pane_hides_input {
        toast::draw_toast(f, outer[layout.transcript_idx], app);
    }
    if let Some(i) = layout.slots.approval {
        approval_draw(f, app, outer[i]);
        panes::stash_approval_rows(f, outer[i], app);
    }
    if let Some(i) = layout.slots.ask {
        super::ask_question::draw(f, app, outer[i]);
        // The card is text the user reads and may want to copy, so it
        // publishes its rows and rect like the approval card beside it.
        // Without this the selection router has no rect to hit and every
        // press falls through.
        panes::stash_approval_rows(f, outer[i], app);
    }
    // The @ skill picker and the / palette share the overlay slot —
    // mutually exclusive (can't have both open). The picker is inline
    // (pushes transcript up) rather than floating over it, matching the
    // palette's layout. The resume picker renders as a Pane instead.
    if let Some(i) = layout.slots.overlay {
        if app.ui.navigation.palette.open {
            palette::draw(f, app, outer[i]);
        } else if app.ui.navigation.skill_picker.open {
            skill_picker::draw(f, app, outer[i]);
        } else {
            input_bar::draw_inline_search(f, outer[i], app);
        }
    }
    if let Some(i) = layout.slots.fleet {
        app.ui.frame.layout.fleet_rect.set(outer[i]);
        fleet_strip::draw(f, outer[i], app);
    }
    if !pane_hides_input {
        input_bar::draw_input(f, outer[layout.input_idx], app);
    }
    if let Some(i) = layout.slots.queue {
        queue_overlay::draw_strip(f, outer[i], app);
    }
    status::draw_status_bar(f, outer[layout.status_idx], app);
    stash_status_rows(f, outer[layout.status_idx], app);
}

/// Stash the rendered status bar rows + rect so the in-app selection path can
/// reach the text the status bar drew. Mirrors stash_pane_rows: the status
/// bar renders through Paragraph widgets, so the rendered cells are the single
/// source of truth for the model/mode/context text the user sees. Trailing
/// blanks are trimmed per row so a drag to the right edge past text does not
/// pad the clipboard. The status bar is one row; multi-row layouts keep the
/// same per-row readback.
fn stash_status_rows(f: &mut Frame, area: Rect, app: &App) {
    app.ui.frame.layout.status_rect.set(area);
    let mut rows = app.ui.frame.layout.last_status_rows.borrow_mut();
    rows.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let buf = f.buffer_mut();
    for ry in 0..area.height {
        let y = area.y + ry;
        let mut row = String::with_capacity(area.width as usize);
        for rx in 0..area.width {
            let x = area.x + rx;
            if let Some(cell) = buf.cell((x, y)) {
                row.push_str(cell.symbol());
            }
        }
        rows.push((TAG_PLAIN, row.trim_end().to_string()));
    }
}

/// The Working-mode vertical layout: a stack of optional + fixed regions
/// split into rects once so each draw path indexes the same slot. Extracted
/// from draw_working so the draw function stays under the line-count gate.
/// The banner leads only when a teammate is in view; otherwise the
/// transcript takes the whole top.
struct WorkingLayout {
    constraints: Vec<Constraint>,
    /// Optional slot indices, packed so the layout stays a small struct.
    slots: LayoutSlots,
    transcript_idx: usize,
    input_idx: usize,
    status_idx: usize,
}

/// The optional-region slot indices. Each is Some only when the region's
/// height is non-zero; None means no slot was reserved. Kept as a named
/// sub-struct so the optional slots read as a cohesive group.
struct LayoutSlots {
    banner: Option<usize>,
    approval: Option<usize>,
    ask: Option<usize>,
    overlay: Option<usize>,
    fleet: Option<usize>,
    queue: Option<usize>,
}

/// The rows the palette and the skill picker take when one of them is open.
/// The reservation and the cell that draws it read this one figure, so they
/// cannot drift apart.
const PALETTE_ROWS: u16 = 10;

/// The shortest the input box may be: one content row between its borders. A
/// shorter box has nowhere to put the caret.
const MIN_INPUT_ROWS: u16 = 3;

fn palette_open(app: &App) -> bool {
    app.ui.navigation.palette.open || app.ui.navigation.skill_picker.open
}

/// Build the Working-mode constraint stack. Each optional region appends a
/// Length cell only when its height is non-zero and records the slot index;
/// the transcript is the fixed Min(1) remainder. Indices are positions in the
/// returned constraints vec, so the caller splits once and indexes by them.
#[allow(clippy::too_many_lines)]
fn build_working_layout(
    app: &App,
    area: Rect,
    input_h: u16,
    queue_h: u16,
    fleet_h: u16,
) -> WorkingLayout {
    let mut constraints: Vec<Constraint> = vec![];
    let banner_h = if app.ui.teammate_view.is_some() {
        3u16
    } else {
        0u16
    };
    let banner = if banner_h > 0 {
        constraints.push(Constraint::Length(banner_h));
        Some(constraints.len() - 1)
    } else {
        None
    };
    constraints.push(Constraint::Min(1));
    let transcript_idx = constraints.len() - 1;
    let status_h = if pane_hides_status(app) { 0 } else { 1 };
    // The card shows its text by default, so a long question can ask for more
    // rows than the terminal has. It may use what is left once the composer
    // keeps its minimum and the transcript its floor: the keys that answer the
    // card come from the card, but the user still needs somewhere to see the
    // conversation it is blocking.
    let ask_h = if let Some(aq) = app.ask_question() {
        let room = area
            .height
            .saturating_sub(MIN_INPUT_ROWS + status_h + banner_h + 1);
        aq.card_height_for(area.width).min(room.max(1))
    } else {
        0u16
    };
    // The approval card may use only what is left once every other region
    // holds its rows: the status bar, the pinned strips, the open overlay, and
    // the transcript minimum of one row. Counting all of them is what keeps
    // the card's rows from being taken out of the input box by the layout
    // solver. Its optional rows give back what does not fit.
    let overlay_h = if palette_open(app) { PALETTE_ROWS } else { 0 };
    let fixed = status_h + banner_h + ask_h + fleet_h + queue_h + overlay_h;
    // The question and the numbered options are the card's own content and
    // cannot be given back, so they outrank rows of draft the user is not
    // editing: the run is blocked until the card is answered, and the keys
    // that answer it are the ones it draws. The composer keeps its minimum
    // height and releases the rest of its draft rows.
    let card_floor = app.approval().map(approval_floor).unwrap_or(0);
    let input_h = if card_floor > 0 {
        // What the composer may keep once the card and the transcript floor
        // hold their rows, never below its own minimum unless the terminal is
        // too short for that minimum either.
        let spare = area.height.saturating_sub(fixed + card_floor + 1);
        let ceiling = area.height.saturating_sub(fixed + 1);
        input_h.min(spare.max(MIN_INPUT_ROWS).min(ceiling))
    } else {
        input_h
    };
    // The card takes its rows from what the composer released and from the
    // transcript, which keeps its floor of one row. A terminal too short for
    // that leaves the card fewer rows than its question and list need; the
    // card then shows the option the keys act on instead of a partial list.
    let approval_room = area.height.saturating_sub(input_h + fixed + 1);
    let approval_h = app
        .approval()
        .map(|approval| approval_card_height(approval, area.width, approval_room, area.height))
        .unwrap_or(0);
    let approval = if approval_h > 0 {
        constraints.push(Constraint::Length(approval_h));
        Some(constraints.len() - 1)
    } else {
        None
    };
    let ask = if ask_h > 0 {
        constraints.push(Constraint::Length(ask_h));
        Some(constraints.len() - 1)
    } else {
        None
    };
    let mut overlay: Option<usize> = None;
    if palette_open(app) {
        constraints.push(Constraint::Length(PALETTE_ROWS));
        overlay = Some(constraints.len() - 1);
    }
    let fleet = if fleet_h > 0 {
        constraints.push(Constraint::Length(fleet_h));
        Some(constraints.len() - 1)
    } else {
        None
    };
    let queue = if queue_h > 0 {
        constraints.push(Constraint::Length(queue_h));
        Some(constraints.len() - 1)
    } else {
        None
    };
    constraints.push(Constraint::Length(input_h));
    let input_idx = constraints.len() - 1;
    constraints.push(Constraint::Length(status_h));
    let status_idx = constraints.len() - 1;
    WorkingLayout {
        constraints,
        slots: LayoutSlots {
            banner,
            approval,
            ask,
            overlay,
            fleet,
            queue,
        },
        transcript_idx,
        input_idx,
        status_idx,
    }
}

/// Focus mode layout: the active pane renders full-width with a 1-line status
/// bar. The pane border title fuses the progress bar (see capability::
/// draw_diff_full and focus_titled_block). The status bar carries the
/// actionable keys (a/r/i) plus the progress bar.
fn draw_focus(f: &mut Frame, app: &App) {
    super::reset_click_regions(app);
    app.ui.frame.feedback.terminal_rows.set(f.area().height);
    let total_h = f.area().height;
    let queue_h = footer_budget::allocate(total_h, 0, queue_overlay::strip_want(app), 0).queue;
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
    stash_status_rows(f, outer[status_idx], app);
}

/// Scroll mode layout: the transcript fills the screen for full-history
/// reading, with a 1-line overlay status bar showing the line position plus
/// search/tail hints.
fn draw_scroll(f: &mut Frame, app: &App) {
    super::reset_click_regions(app);
    app.ui.frame.feedback.terminal_rows.set(f.area().height);
    let total_h = f.area().height;
    let queue_h = footer_budget::allocate(total_h, 0, queue_overlay::strip_want(app), 0).queue;
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
    if app.window_browsing() {
        history_window::draw_history_window(f, outer[0], app);
    } else {
        transcript::draw_transcript(f, outer[0], app);
    }
    // Notices overlay the transcript's bottom row in this mode as they do in
    // working mode: a notice raised while scrolling (a read that failed, a copy
    // that landed) has no other row to reach the reader on, and a destination
    // no view draws is a failure the reader never learns about.
    toast::draw_toast(f, outer[0], app);
    if let Some(i) = queue_idx {
        queue_overlay::draw_strip(f, outer[i], app);
    }
    status::draw_scroll_status(f, outer[status_idx], app);
    stash_status_rows(f, outer[status_idx], app);
}

/// Render the active pane full-width in Focus mode (a full-width code area).
fn draw_focus_main(f: &mut Frame, area: Rect, app: &App) {
    match app.ui.navigation.pane {
        Pane::Diff => capability::draw_diff_full(f, area, app),
        Pane::Transcript => transcript::draw_transcript(f, area, app),
        Pane::Memory => panes::draw_memory_pane(f, area, app),
        Pane::Sandbox => panes::draw_sandbox_pane(f, area, app),
        Pane::Worktree => panes::draw_worktree_pane(f, area, app),
        Pane::Trajectory => panes::draw_trajectory_pane(f, area, app),
        Pane::Status => panes::draw_status_pane(f, area, app),
        Pane::Resume => panes::draw_resume_pane(f, area, app),
        Pane::Hooks => panes::draw_hooks_pane(f, area, app),
        Pane::Skills => panes::draw_skills_pane(f, area, app),
        Pane::Tasks => panes::draw_tasks_pane(f, area, app),
        Pane::Model => panes::draw_model_pane(f, area, app),
        Pane::Agents => panes::draw_agents_pane(f, area, app),
        Pane::Queue => panes::draw_queue_pane(f, area, app),
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
    if matches!(app.ui.navigation.pane, Pane::Permission) {
        panes::draw_permission_pane(f, area, app);
        return;
    }
    // The /memory pane renders inline (the Pane primitive, shared with
    // /permissions and /search): transcript tail above, the memory list +
    // toggle rows in the lower band. NOT an external editor — the user stays
    // in-TUI, mirroring /permissions and /search. One path for stub + wired.
    if matches!(app.ui.navigation.pane, Pane::Memory) {
        panes::draw_memory_pane(f, area, app);
        return;
    }
    // The /worktrees pane renders inline (the Pane primitive, shared with
    // /permissions /search /memory): transcript tail above, the worktree list
    // in the lower band. The worktree-list surface has no
    // equivalent command elsewhere, so this pane is the in-TUI manager. Selection +
    // copy come free via the shared PaneRegion (the mouse router keys on
    // the pane rect, which draw_command_pane stashes).
    if matches!(app.ui.navigation.pane, Pane::Worktree) {
        panes::draw_worktree_pane(f, area, app);
        return;
    }
    // The /sandbox pane renders inline (the Pane primitive, shared with
    // /permissions /worktrees /memory): transcript tail above, the
    // boundaries or the recorded checkpoints in the lower band.
    if matches!(app.ui.navigation.pane, Pane::Sandbox) {
        panes::draw_sandbox_pane(f, area, app);
        return;
    }
    if matches!(app.ui.navigation.pane, Pane::Trajectory) {
        panes::draw_trajectory_pane(f, area, app);
        return;
    }
    // The /status pane renders inline (the Pane primitive, shared with
    // /permissions /memory /worktrees /resume): transcript tail above, the
    // status fields below. NOT a transcript text dump — the command opens a
    // live pane so Esc dismisses + the field set stays visible while the
    // cache refreshes.
    if matches!(app.ui.navigation.pane, Pane::Status) {
        panes::draw_status_pane(f, area, app);
        return;
    }
    // The /resume picker renders inline (the Pane primitive): transcript tail
    // above, the filtered session list below. NOT a bottom-anchored popover;
    // the shared template keeps the transcript tail visible + selection +
    // copy working for free.
    if matches!(app.ui.navigation.pane, Pane::Resume) {
        panes::draw_resume_pane(f, area, app);
        return;
    }
    // The /hooks pane renders inline (the Pane primitive): transcript tail
    // above, the hook list below.
    if matches!(app.ui.navigation.pane, Pane::Hooks) {
        panes::draw_hooks_pane(f, area, app);
        return;
    }
    // The /skills pane renders inline (the Pane primitive): transcript tail
    // above, the discovered-skill list below.
    if matches!(app.ui.navigation.pane, Pane::Skills) {
        panes::draw_skills_pane(f, area, app);
        return;
    }
    // The /tasks pane renders inline through the same template: transcript
    // tail above, the background-command list or the open detail below.
    if matches!(app.ui.navigation.pane, Pane::Tasks) {
        panes::draw_tasks_pane(f, area, app);
        return;
    }
    // The /model pane renders inline (the Pane primitive): transcript tail
    // above, the selectable model list below.
    if matches!(app.ui.navigation.pane, Pane::Model) {
        panes::draw_model_pane(f, area, app);
        return;
    }
    // The /agents pane renders inline (the Pane primitive): transcript tail
    // above, the live children, the returned delegations, and the delegatable
    // types below.
    if matches!(app.ui.navigation.pane, Pane::Agents) {
        panes::draw_agents_pane(f, area, app);
        return;
    }
    // The queue pane renders inline (the Pane primitive): transcript tail
    // above, the queued-item list below. Opened by clicking the collapsed
    // +N more strip, not by a slash command.
    if matches!(app.ui.navigation.pane, Pane::Queue) {
        panes::draw_queue_pane(f, area, app);
        return;
    }
    if app.sessions.current.connection.is_some() {
        // Full-screen overlays that take over the main area in agent-chat
        // mode (otherwise the main area is the chat transcript). The artifact
        // pane is the slash-command surface that owns the whole main area when
        // open.
        if matches!(app.ui.navigation.pane, Pane::Artifact) {
            artifact::draw(f, area, app);
            return;
        }
        transcript::draw_transcript(f, area, app);
        return;
    }
    if matches!(app.ui.navigation.pane, Pane::Diff) {
        capability::draw_diff_full(f, area, app);
        return;
    }
    if matches!(app.ui.navigation.pane, Pane::Transcript) {
        transcript::draw_transcript(f, area, app);
        return;
    }
    if matches!(app.ui.navigation.pane, Pane::Artifact) {
        artifact::draw(f, area, app);
        return;
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    transcript::draw_transcript(f, cols[0], app);
    capability::draw(f, cols[1], app);
}

#[cfg(test)]
mod tests;
