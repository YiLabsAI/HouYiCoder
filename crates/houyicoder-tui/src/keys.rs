//! Per-screen and overlay key handlers. Each handler mutates App state in
//! response to a key. The working surface dispatches to palette / approval /
//! input handlers; the input handler also

mod fleet;
mod login;
pub use login::{handle_console, handle_login};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[cfg(test)]
use crate::state::Stage;
use crate::state::{App, Pane, ViewportMode};

mod input;
mod pane_predicates;
use pane_predicates::{
    pane_approvable, pane_navigable, pane_owns_esc, pane_rejectable, pane_reworkable,
};

mod palette;

/// Working surface keys: respect open overlays, else dispatch by viewport
/// mode (Scroll takes over; Focus handles only action keys; Working is full
/// input).
pub fn handle_working(app: &mut App, k: KeyEvent) {
    if app.viewport == ViewportMode::Scroll {
        pager::handle_scroll(app, k);
        return;
    }
    // Self-heal stale queue_view_open so it never captures keys off-render.
    if app.viewport != ViewportMode::Working {
        app.queue_view_open = false;
    }
    // Clear a lingering name editor when focus leaves the Status pane (an
    // approval/Focus takeover or a /command that switches pane would otherwise
    // leave an invisible Some, so the next 'e' inserts into the stale buffer
    // instead of opening fresh. swap_session rebuilds via build_app so
    // it clears too, but this covers the in-session pane switches.
    if app.pane != Pane::Status {
        app.status_name_edit = None;
    }
    // /permission pane: nav and sub-mode entry keys live in the pane's own
    // handler. In a typed sub-mode (add / remove / search) the key falls
    // through so the user can type the spec and Enter to submit.
    if crate::permission_input::handle_permission_key(app, k) {
        return;
    }
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('o') {
        handle_ctrl_o(app);
        return;
    }
    // Ctrl+G toggles the queue overlay (per-item edit/del/recall), replacing
    // the old Esc-pops-queue gesture. Working-only; suppressed while palette/
    // search open so two overlays cannot stack. Opens only on a non-empty
    // queue; closes on a second press.
    if app.viewport == ViewportMode::Working
        && !app.palette.open
        && !app.search.active
        && k.modifiers.contains(KeyModifiers::CONTROL)
        && k.code == KeyCode::Char('g')
    {
        if app.queue_view_open {
            app.queue_view_open = false;
        } else if !app.pending.is_empty() {
            app.queue_focus = 0;
            app.queue_view_open = true;
        }
        return;
    }
    // Shift+Up/Down move the footer-pill fleet selection before input
    // handling so it works even while the input box has focus.
    if fleet::fleet_shift_selected(&mut app.fleet, app.viewport, k) {
        return;
    }
    if app.viewport == ViewportMode::Working && app.queue_view_open && handle_queue_overlay(app, k)
    {
        return;
    }
    // Overlay not consumed: fall through so input-edit keys and Enter (submit)
    // work while the overlay is open.
    // Status-name edit: a focused input mode on the /status Status tab. Takes
    // priority so typed chars edit the buffer instead of cycling tabs or
    // hitting the generic pane arms.
    if app.pane == Pane::Status && app.status_name_edit.is_some() {
        handle_status_name_edit(app, k);
        return;
    }
    if app.resume_picker.open {
        handle_resume_picker(app, k);
        return;
    }
    if app.palette.open {
        palette::handle_palette(app, k);
        return;
    }
    // Esc while viewing a teammate: a running child aborts its current
    // turn (non-terminal); a completed/non-running child exits the view.
    // Requires an empty input box so a mid-type Esc does not yank the view.
    if app.teammate_view.is_some() && k.code == KeyCode::Esc && app.input.is_empty() {
        app.esc_teammate_view_or_abort();
        return;
    }
    // Approval pending: handled inline at the top of handle_input so scroll,
    // palette, and search still work. a/r decide, Enter confirms, Esc dismisses.
    // Startup workspace-trust card fires once before any run; it takes
    // priority over a mid-run approval, so check it first.
    if app.pending_trust.is_some() {
        handle_trust(app, k);
        return;
    }
    if app.viewport == ViewportMode::Focus {
        handle_focus(app, k);
        return;
    }
    // the input box (merged with any draft). This fires after Esc1 interrupted
    // (cancelling, agent_busy still true until Done) or when idle, so a
    // second Esc recalls instead of re-aborting. Panes that own Esc (Memory,
    // Artifact, /trajectory, ...) are gated out via pane_owns_esc so Esc
    // closes or backs the pane first. Suppressed outside Working so a
    // scroll/search Esc is not stolen; Ctrl+G's per-item recall (e) picks a
    // non-head item.
    if (app.cancelling || !app.agent_busy)
        && app.viewport == ViewportMode::Working
        && !app.pending.is_empty()
        && k.code == KeyCode::Esc
        && !pane_owns_esc(app.pane)
    {
        app.pop_queued_to_input();
        return;
    }
    // Esc while a run is in flight interrupts it and nothing else: the queue
    // is left intact and the input draft is untouched. Fires only when not
    // already cancelling (the first Esc); a subsequent Esc during the
    // cancelling window is caught by the recall arm above (pending) or, with
    // no pending, re-aborts here (idempotent -- a lost AbortRun gets a second
    // chance). Panes that own Esc are gated out so Esc closes the pane first.
    if app.agent_busy && k.code == KeyCode::Esc && !pane_owns_esc(app.pane) {
        tracing::debug!("abort_run (busy, queue left intact)");
        app.abort_run();
        return;
    }
    input::handle_input(app, k);
}

/// Keys for the startup workspace-trust card. Enter or y trusts the project
/// (persists the trust + continues); Esc or n declines (the server shuts the
/// session). Fires once at startup before any run, so the dispatch in
/// handle_working checks it before the Focus/approval gate.
fn handle_trust(app: &mut App, k: KeyEvent) {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => app.resolve_trust(true),
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => app.resolve_trust(false),
        _ => {}
    }
}

/// Focus mode keys: input is hidden, so only per-pane action keys (a/r/i),
/// Up/Down, PgUp to scroll, and Esc to fold to Working are accepted.
pub(crate) fn handle_ctrl_o(app: &mut App) {
    if app.toggle_subagent_expand() {
        return;
    }
    if app.toggle_thinking_expand() {
        return;
    }
    let has_fold_key = app
        .anchor_visible_row()
        .and_then(|ri| app.last_row_fold_keys.borrow().get(ri).cloned().flatten())
        .is_some();
    let has_active_todo = !app.todos_cache.is_empty();
    if has_fold_key {
        app.toggle_focused_fold_expand();
    } else if has_active_todo {
        app.todo_expanded = !app.todo_expanded;
    } else {
        app.toggle_focused_result_expand();
    }
}

fn handle_focus(app: &mut App, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => app.fold_to_working(),
        KeyCode::PageUp => app.enter_scroll(),
        KeyCode::Up if pane_navigable(app) => app.navigate_pane(false),
        KeyCode::Down if pane_navigable(app) => app.navigate_pane(true),
        KeyCode::Char('a') if pane_approvable(app) => app.approve_in_pane(),
        KeyCode::Char('r') if pane_rejectable(app) => app.reject_in_pane(),
        KeyCode::Char('r') | KeyCode::Char('i') if pane_reworkable(app) => {
            app.rework_in_pane();
        }
        _ => {}
    }
}

/// Status-name edit keys (the e-to-rename flow on the /status Status tab).
mod status_name_edit;
use status_name_edit::handle_status_name_edit;

/// Session-picker keys: type to filter by sid or name, navigate the list,
/// Enter resumes the selection (in-process swap), Esc closes.
mod resume_picker;
use resume_picker::handle_resume_picker;

/// Queue overlay keys (Ctrl+G opened it). Up/Down move cursor; e recalls
/// focused (removed, overlay closes); d deletes; a recalls all; Esc/Ctrl+G
/// closes. Returns true when consumed; false falls through so Ctrl+A/E/U,
/// arrows, Backspace, and Enter (submit) work while open. Enter is NOT
/// recall; recall is e only.
mod overlay;
mod pager;
use overlay::handle_queue_overlay;

/// Approval prompt and cursor-navigation keys live in keys::approval.
mod approval;

/// AskUserQuestion card key handlers and the /worktrees pane handler, split out
/// to keep this module under the file-size gate.
mod ask_question;
mod trajectory;
mod worktree_pane;

#[cfg(test)]
#[path = "keys_tests.rs"]
mod tests;
