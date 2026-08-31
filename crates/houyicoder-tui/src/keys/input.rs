//! Input box dispatch tree. handle_input is the entry: a per-pane action
//! key handler that may consume the key, falling through to the generic
//! input switch (type, send, open palette, switch pane, per-pane hunk and
//! finding keys). Split from the keys root so that module stays a thin
//! working-surface dispatcher.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::enums::CyclicTab;
use crate::state::{App, Pane};

use super::approval::handle_approval;
use super::ask_question::handle_ask_question;
use super::fleet;
use super::pane_predicates::{
    artifact_editing, pane_approvable, pane_navigable, pane_rejectable, pane_reworkable,
};
use super::worktree_pane;

/// Lines moved per PageUp/PageDown in the artifact pane. Fixed rather than
/// view-height-aware to keep the page handler out of the render loop.
const ARTIFACT_PAGE_SIZE: usize = 10;

/// Input box keys: type, send, open palette, switch pane, toggle plan mode,
/// and per-pane hunk/finding keys.
pub(super) fn handle_input(app: &mut App, k: KeyEvent) {
    if let Some(remaining) = pane_action_key(app, k) {
        // The key was not consumed by the pane action controls; fall through
        // to the generic input handler.
        handle_generic_input(app, remaining);
    }
}

/// Per-pane action keys. When the input is empty and the pane is in a stage
/// that accepts an action, a approves, r rejects, Up/Down move focus. In the
/// artifact pane Normal mode, c/o/i enter an edit mode and d proposes an
/// immediate delete; in an edit mode, Esc cancels to Normal. Returns the key
/// to keep processing when not consumed here.
fn pane_action_key(app: &mut App, k: KeyEvent) -> Option<KeyEvent> {
    let empty = app.input.is_empty();
    // Esc in an artifact edit mode cancels to Normal (clearing any in-progress
    // text). Esc in Normal mode falls through to handle_generic_input, which
    // exits the pane to Transcript.
    if app.pane == Pane::Artifact && !app.artifact.mode().is_normal() {
        if k.code == KeyCode::Esc {
            app.artifact.cancel_mode();
            app.input.clear();
            return None;
        }
        // In an edit mode, all other keys fall through to the generic input
        // handler so the user can type the edit text and Enter to submit.
        return Some(k);
    }
    match k.code {
        KeyCode::Up if pane_navigable(app) => {
            app.navigate_pane(false);
            return None;
        }
        KeyCode::Down if pane_navigable(app) => {
            app.navigate_pane(true);
            return None;
        }
        // In the artifact pane, PageUp/PageDown page the line cursor instead
        // of entering transcript scroll, so a long document stays navigable.
        KeyCode::PageUp if app.pane == Pane::Artifact => {
            app.artifact.focus_page_up(ARTIFACT_PAGE_SIZE);
            return None;
        }
        KeyCode::PageDown if app.pane == Pane::Artifact => {
            app.artifact.focus_page_down(ARTIFACT_PAGE_SIZE);
            return None;
        }
        // Artifact Normal mode edit keys (only when input is empty, so mid-
        // typing c/o/d/i go to the input box instead). c/o/i enter an edit
        // mode; d proposes an immediate delete of the focused line.
        KeyCode::Char('c') if empty && app.pane == Pane::Artifact => {
            app.artifact.enter_replace();
            return None;
        }
        KeyCode::Char('o') if empty && app.pane == Pane::Artifact => {
            app.artifact.enter_insert();
            return None;
        }
        KeyCode::Char('i') if empty && app.pane == Pane::Artifact => {
            app.artifact.enter_nl();
            return None;
        }
        KeyCode::Char('d') if empty && app.pane == Pane::Artifact => {
            app.artifact_propose_delete();
            return None;
        }
        KeyCode::Char('a') if empty && pane_approvable(app) => {
            app.approve_in_pane();
            return None;
        }
        KeyCode::Char('r') if empty && pane_rejectable(app) => {
            app.reject_in_pane();
            return None;
        }
        KeyCode::Char('r') | KeyCode::Char('i') if empty && pane_reworkable(app) => {
            app.rework_in_pane();
            return None;
        }
        _ => {}
    }
    Some(k)
}

/// Generic input handler: quit, cycle pane, replay, palette, submit, etc.
/// Shortcut arms are gated on !artifact_editing so an in-progress edit is not
/// derailed by a first-character shortcut match (q would quit, Tab would flee).
#[expect(clippy::cognitive_complexity, reason = "dispatch tree")]
#[expect(clippy::too_many_lines, reason = "dispatch tree")]
fn handle_generic_input(app: &mut App, k: KeyEvent) {
    // Permission pane browsing mode (not in Add/AddDir sub-mode): swallow
    // all input. When Add is active, permission_input.is_active() returns
    // true, and the chars must reach app.input (the user types the rule spec).
    if app.pane == Pane::Permission && !app.permission_input.is_active() {
        return;
    }
    // Approval pending: a/r decide, Enter confirms the focus, Esc dismisses.
    // Other typing is ignored while the run is paused on the permission gate
    // (scroll/palette/search are handled earlier so they still work).
    if app.approval.is_some() {
        handle_approval(app, k);
        return;
    }
    // AskUserQuestion pending: arrows navigate options, Enter selects/toggles,
    // Esc cancels. When Other is focused, typing goes into the text input.
    if app.ask_question.is_some() {
        handle_ask_question(app, k);
        return;
    }
    let editing = artifact_editing(app);
    match k.code {
        KeyCode::Char('q') if app.input.is_empty() && !editing => app.quit = true,
        // Esc leaves the artifact pane back to the conversation (the main
        // view). Only when the input is empty so mid-typing Esc is a no-op
        // (the user can still Backspace to clear, then Esc to exit). Esc in
        // an edit mode is caught earlier by pane_action_key (cancel to Normal).
        KeyCode::Esc if app.pane == Pane::Artifact && app.input.is_empty() => {
            app.pane = Pane::Transcript;
        }
        // Esc in the base working state (no overlay, no artifact edit): clear
        // the input box if it has text, otherwise no-op. Gives Esc a concrete
        // behavior instead of a dead key.
        KeyCode::Esc if !app.input.is_empty() => {
            app.input.clear();
        }
        // In the /status pane, Tab cycles the sub-tab (Status, Config, Usage)
        // instead of the whole pane (the pane owns its tab bar).
        KeyCode::Tab if app.pane == Pane::Status => app.status_tab = app.status_tab.next(),
        // On the /status Status tab, the e key opens the inline session-name
        // editor (houyi makes the session name
        // inline-editable rather than a rename command). Gated on the Status tab and an empty input box so
        // the palette-style input path stays the fallback elsewhere.
        KeyCode::Char('e')
            if app.pane == Pane::Status
                && app.status_tab == crate::state::enums::StatusTab::Status
                && app.status_name_edit.is_none()
                && app.input.is_empty() =>
        {
            app.enter_status_name_edit();
        }
        KeyCode::Tab if !editing => cycle_pane(app),
        // /memory pane: Up/Down move the cursor, d forgets the row under it,
        // enter shows the body, Left/Right cycle the storage-scope filter,
        // Esc clears the text filter (else leaves). Gated on the pane so the
        // keys keep their normal meaning elsewhere. Left/Right match the
        // /permissions pane's tab switching; Shift+Tab stays the global
        // mode cycle instead of being shadowed by this pane.
        KeyCode::Left if app.pane == Pane::Memory && app.input.is_empty() => {
            app.cycle_memory_scope_prev()
        }
        KeyCode::Right if app.pane == Pane::Memory && app.input.is_empty() => {
            app.cycle_memory_scope()
        }
        KeyCode::Up if app.pane == Pane::Memory => app.move_memory_cursor(-1),
        KeyCode::Down if app.pane == Pane::Memory => app.move_memory_cursor(1),
        KeyCode::Up if app.pane == Pane::Agents && !app.fleet.entries.is_empty() => {
            app.fleet.move_selection(-1);
        }
        KeyCode::Down if app.pane == Pane::Agents && !app.fleet.entries.is_empty() => {
            app.fleet.move_selection(1);
        }
        // Shift+Tab cycles the permission mode: default, auto, bypass, default. No pane shadows it now (the /memory scope filter moved to
        // Left/Right), so Shift+Tab is always the global mode cycle.
        KeyCode::BackTab => app.tab_cycle_mode(),
        KeyCode::Char('d') if app.pane == Pane::Memory && app.input.is_empty() => {
            app.forget_memory_at_cursor()
        }
        KeyCode::Enter if app.pane == Pane::Memory && app.input.is_empty() => {
            app.show_memory_at_cursor()
        }
        KeyCode::Esc if app.pane == Pane::Memory && app.memory_list.searching() => {
            app.memory_list.clear_query();
            app.memory_list.cursor = 0;
        }
        // Esc on the /memory pane with no text filter dismisses the pane back
        // to the transcript. The pane footer advertises "Esc close", so the
        // key must actually close it; without this arm Esc was a dead key
        // when the input box and the search filter were both empty. Matches
        // the Artifact pane's Esc-to-transcript behavior.
        KeyCode::Esc if app.pane == Pane::Memory => {
            app.pane = Pane::Transcript;
        }
        // /worktrees pane: delegated to keys/worktree_pane.rs.
        _ if app.pane == Pane::Worktree && worktree_pane::handle(app, k) => {}
        // /status pane: Left/Right (or Tab) cycle the sub-tab (Status,
        // Config, Usage), Esc dismisses back to the transcript. A
        // Settings-modal-style tabbed status.
        KeyCode::Left if app.pane == Pane::Status => app.status_tab = app.status_tab.prev(),
        KeyCode::Right if app.pane == Pane::Status => app.status_tab = app.status_tab.next(),
        KeyCode::Esc if app.pane == Pane::Status => {
            app.pane = Pane::Transcript;
        }
        KeyCode::Down if app.pane == Pane::Model => {
            let len = crate::view::model_pane::model_row_count(app);
            app.model_sel = (app.model_sel + 1).min(len.saturating_sub(1));
            app.recompute_effort_on_cursor_move();
        }
        KeyCode::Up if app.pane == Pane::Model => {
            app.model_sel = app.model_sel.saturating_sub(1);
            app.recompute_effort_on_cursor_move();
        }
        KeyCode::Left if app.pane == Pane::Model => {
            app.cycle_effort(false);
        }
        KeyCode::Right if app.pane == Pane::Model => {
            app.cycle_effort(true);
        }
        KeyCode::Enter if app.pane == Pane::Model && app.input.is_empty() => {
            app.set_model_at_cursor();
        }
        KeyCode::Esc if app.pane == Pane::Model => {
            app.pane = Pane::Transcript;
        }
        KeyCode::Esc if app.pane == Pane::Skills => {
            app.pane = Pane::Transcript;
        }
        KeyCode::Esc if app.pane == Pane::Hooks => {
            if app.hooks_level.get() > 0 {
                app.hooks_level.set(0);
            } else {
                app.pane = Pane::Transcript;
            }
        }
        KeyCode::Up if app.pane == Pane::Hooks && app.hooks_level.get() == 0 => {
            let cur = app.hooks_sel.get();
            app.hooks_sel.set(cur.saturating_sub(1));
        }
        KeyCode::Down if app.pane == Pane::Hooks && app.hooks_level.get() == 0 => {
            let len = app
                .hook_entries
                .iter()
                .filter(|h| h.source == "framework")
                .count();
            let next = app.hooks_sel.get() + 1;
            app.hooks_sel.set(next.min(len.saturating_sub(1)));
        }
        KeyCode::Enter if app.pane == Pane::Hooks && app.hooks_level.get() == 0 => {
            app.hooks_level.set(1);
        }
        // /trajectory pane: 3-level drill-down.
        // Level 0: turn list: Up/Down select, Enter expands, Esc closes.
        // Level 1: turn detail: Up/Down select events, Enter shows detail, Esc back.
        // Level 2: event detail: Esc back to level 1.
        KeyCode::Up if app.pane == Pane::Trajectory => {
            // Level 2 is a stable detail view (the event selected at L1),
            // not a switcher, Up/Down is a no-op there; switch events at L1.
            if app.trajectory_level.get() < 2 {
                let c = app.trajectory_cursor.get();
                app.trajectory_cursor.set(c.saturating_sub(1));
            }
        }
        KeyCode::Down if app.pane == Pane::Trajectory => {
            if app.trajectory_level.get() < 2 {
                // Clamp to [0, len-1] so the selection glyph stays on the last
                // row instead of vanishing past the end. len is stashed by the
                // render path (draw_content); 0 before first render = unbounded.
                let c = app.trajectory_cursor.get();
                let len = app.trajectory_list_len.get();
                let next = c + 1;
                let max = if len == 0 {
                    next
                } else {
                    len.saturating_sub(1)
                };
                app.trajectory_cursor.set(next.min(max));
            }
        }
        KeyCode::Enter if app.pane == Pane::Trajectory && app.input.is_empty() => {
            let level = app.trajectory_level.get();
            if level == 0 && app.trajectory_list_len.get() > 0 {
                // Freeze the turn-list selection so the turn-detail and
                // event-detail levels render THAT row, not the first turn.
                // Works for both Turn and [bg] rows. Skip the drill when the
                // row list is empty (a fresh session with no turns yet);
                // drilling into no rows rendered "no row data" at the
                // turn-detail level, which read as a crash.
                app.trajectory_turn_idx.set(app.trajectory_cursor.get());
                app.trajectory_level.set(1);
                app.trajectory_cursor.set(0);
            } else if level == 1 {
                // [bg] rows have no event list to drill into; stay at L1.
                if !app.trajectory_at_bg.get() {
                    app.trajectory_level.set(2);
                    // Keep the cursor so L2 shows the event selected at L1.
                }
            }
        }
        KeyCode::Esc if app.pane == Pane::Trajectory => {
            let level = app.trajectory_level.get();
            if level == 0 {
                app.pane = Pane::Transcript;
            } else {
                app.trajectory_level.set(level - 1);
                app.trajectory_cursor.set(0);
            }
        }
        // '/' opens the slash palette only when no typed permission sub-mode
        // (add rule / add directory) is active; an AddDir path typically
        // starts with '/', and the leading slash must land in the input box,
        // not snap to the palette. The other sub-modes own their keys
        // (handle_permission_key consumes them) so they never reach here.
        KeyCode::Char('/')
            if app.input.is_empty()
                && !editing
                && !app.permission_input.is_active()
                && !matches!(
                    app.pane,
                    Pane::Model
                        | Pane::Hooks
                        | Pane::Status
                        | Pane::Memory
                        | Pane::Worktree
                        | Pane::Trajectory
                        | Pane::Resume
                ) =>
        {
            app.open_palette()
        }
        KeyCode::Enter
            if !matches!(
                app.pane,
                Pane::Model
                    | Pane::Hooks
                    | Pane::Status
                    | Pane::Memory
                    | Pane::Worktree
                    | Pane::Trajectory
                    | Pane::Resume
            ) =>
        {
            // Empty-input Enter drills into a teammate view: the pill
            // selection takes priority, then the Subagent line at cursor.
            if app.input.is_empty() && app.teammate_view.is_none() {
                if let Some(sid) = fleet::selected_fleet_sid(app) {
                    app.enter_teammate_view_for_sid(&sid, true);
                    return;
                }
                if app.enter_teammate_view() {
                    return;
                }
            }
            app.submit_input()
        }
        KeyCode::PageUp if !editing => {
            // Enter Scroll mode without paging: the first PgUp from Working
            // mode switches to the full-screen transcript so the render can
            // publish the Scroll-mode cap (larger than Working's). Subsequent
            // PgUps in Scroll mode page with the correct cap. Paging here
            // with the stale Working cap set the offset too high, pushing the
            // block entirely above the visible window.
            app.enter_scroll();
        }
        KeyCode::PageDown if !editing => app.scroll_transcript_down(),
        // Ctrl+End always re-follows the tail, even mid-edit, so the user can
        // jump to the newest content without submitting or clearing input.
        KeyCode::End if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.scroll_transcript_follow_tail();
        }
        KeyCode::End if !editing && app.input.is_empty() => app.scroll_transcript_follow_tail(),
        // Cursor movement within the (possibly wrapped) input. Home/End and
        // Ctrl+A/E operate on the visual wrapped line (readline semantics),
        // not the whole text; double-press Home walks up wrapped lines.
        // Up/Down use the wrap column count stashed by the draw pass.
        KeyCode::Left => app.input.move_left(),
        KeyCode::Right => app.input.move_right(),
        KeyCode::Up => app.input.move_up(app.last_cols.get()),
        KeyCode::Down => app.input.move_down(app.last_cols.get()),
        KeyCode::Home => app.input.move_line_home(app.last_cols.get()),
        KeyCode::End => app.input.move_line_end(app.last_cols.get()),
        KeyCode::Char('a') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.move_home();
        }
        KeyCode::Char('e') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.move_end();
        }
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.kill_to_line_start(app.last_cols.get());
        }
        KeyCode::Backspace
            if !matches!(
                app.pane,
                Pane::Model
                    | Pane::Hooks
                    | Pane::Status
                    | Pane::Memory
                    | Pane::Worktree
                    | Pane::Trajectory
                    | Pane::Resume
            ) =>
        {
            app.input.pop()
        }
        KeyCode::Char(c)
            if !matches!(
                app.pane,
                Pane::Model
                    | Pane::Hooks
                    | Pane::Status
                    | Pane::Memory
                    | Pane::Worktree
                    | Pane::Trajectory
                    | Pane::Resume
            ) =>
        {
            app.input.push(c)
        }
        _ => {}
    }
}

/// Cycle the capability pane forward through the Tab order (the six primary
/// guided-chain panes only). Utility panes are slash-only. In agent-chat mode
/// (runner wired) Tab is a no-op: the working screen is a single chat stream,
/// not a pane tab system.
pub(super) fn cycle_pane(app: &mut App) {
    if app.session.is_some() {
        return;
    }
    let next = Pane::PRIMARY
        .iter()
        .position(|&p| p == app.pane)
        .map(|i| (i + 1) % Pane::PRIMARY.len())
        .unwrap_or(0);
    app.pane = Pane::PRIMARY[next];
}
