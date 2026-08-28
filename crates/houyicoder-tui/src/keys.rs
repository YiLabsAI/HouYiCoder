//! Per-screen and overlay key handlers. Each handler mutates App state in
//! response to a key. All actions are placeholders. The working surface
//! dispatches to palette / approval / input handlers; the input handler also

mod fleet;
mod login;
pub use login::{handle_console, handle_login};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[cfg(test)]
use crate::state::Stage;
use crate::state::enums::CyclicTab;
use crate::state::{App, Pane, ViewportMode};

mod pane_predicates;
use pane_predicates::{
    artifact_editing, pane_approvable, pane_navigable, pane_rejectable, pane_reworkable,
};

/// Lines moved per PageUp/PageDown in the artifact pane. Fixed rather than
/// view-height-aware to keep the page handler out of the render loop.
const ARTIFACT_PAGE_SIZE: usize = 10;

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
    // /permission pane: nav + sub-mode entry keys live in the pane's own
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
        handle_palette(app, k);
        return;
    }
    // Esc while viewing a teammate transcript exits to the parent. Sync
    // spawn children are always completed, so Esc always exits. A non-empty
    // input clears first via the generic arm below.
    if app.teammate_view.is_some() && k.code == KeyCode::Esc && app.input.is_empty() {
        app.exit_teammate_view();
        return;
    }
    // Approval pending: handled inline at the top of handle_input so scroll,
    // palette, and search still work. a/r decide, Enter confirms, Esc dismisses.
    if app.viewport == ViewportMode::Focus {
        handle_focus(app, k);
        return;
    }
    // Esc while a run is in flight aborts it, but only when the input box
    // is empty and no pane with its own Esc-close is open. A non-empty
    // input during a run is a queued draft; Esc clears it first, so a
    // second Esc aborts. Panes with their own Esc (Memory, Artifact) are
    // gated out so Esc closes the pane first.
    if app.agent_busy
        && k.code == KeyCode::Esc
        && app.input.is_empty()
        && !matches!(
            app.pane,
            Pane::Memory
                | Pane::Artifact
                | Pane::Worktree
                | Pane::Status
                | Pane::Resume
                | Pane::Hooks
                | Pane::Skills
                | Pane::Model
        )
    {
        // Esc to interrupt a busy run. If the queue holds a pending input,
        // pop its head to the input box NOW (one Esc = abort + pop): the
        // un-executed item is the user's pending intent, so on a redirect it
        // comes back for editing + re-send, not auto-fired (the run ends
        // Interrupted, the clean-end gate holds, remaining items park).
        if !app.pending.is_empty() {
            tracing::debug!("abort_run + pop_queued (busy + queued)");
            app.abort_run();
            app.pop_queued_to_input();
        } else {
            tracing::debug!("abort_run (busy + empty input)");
            app.abort_run();
        }
        return;
    }
    // Idle + a parked queued input (a non-clean run end left it, not
    // auto-sent): Esc pops the head into the input box for editing (no
    // running task to abort). Suppressed outside Working so a scroll/search
    // Esc is not stolen; Ctrl+G's per-item recall (e) picks a non-head item.
    if !app.agent_busy
        && app.viewport == ViewportMode::Working
        && !app.pending.is_empty()
        && app.input.is_empty()
        && k.code == KeyCode::Esc
    {
        app.pop_queued_to_input();
        return;
    }
    handle_input(app, k);
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

/// Slash-command palette keys: type to filter, navigate the filtered list,
/// select, or close. Backspace edits the query; Esc closes.
fn handle_palette(app: &mut App, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => app.close_palette(),
        KeyCode::Up => app.palette_up(),
        KeyCode::Down => app.palette_down(),
        KeyCode::Enter => {
            if let Some(cmd) = app.selected_command() {
                // Arg-taking commands: keep the palette OPEN and seed the query
                // with the name + a trailing space, so the user keeps typing
                // the argument in the popup (which shows the arg hint after the
                // space). The popup stays open after a space: a typical
                // input-box autocomplete vanishes after a space, leaving blind
                // typing; our popup stays open and guides the argument. Argless
                // commands auto-run on select.
                if cmd.takes_arg() {
                    app.palette.query = format!("{} ", cmd.name().trim_start_matches('/'));
                    app.palette.sel = 0;
                } else {
                    app.close_palette();
                    app.input.set(cmd.name().to_string());
                    app.submit_input();
                }
            } else if !app.palette.query.is_empty() {
                // No palette entry matches the query. Submit it as a slash
                // command so arg-bearing forms typed in the filter
                // (/permissions git off, /resume file.json) and genuinely
                // unknown tokens (/nope) still run instead of dropping Enter on
                // the floor. Strip any leading slash the user typed so it does
                // not double-slash.
                let q = app.palette.query.trim_start_matches('/');
                let raw = format!("/{q}");
                app.close_palette();
                app.input.set(raw);
                app.submit_input();
            }
        }
        KeyCode::Backspace => {
            if app.palette.query.is_empty() {
                app.close_palette();
            } else {
                app.palette_pop();
            }
        }
        // Accept ascii-graphic chars + the space separator. The space is the
        // arg separator for arg-taking local commands (/permissions git off):
        // with no palette entry matching a spaced query, Enter falls through
        // to the raw-submit branch + ships the typed query as a slash command.
        // Without accepting the space, arg-taking commands are unreachable
        // (the query arrives concatenated, e.g. "permissionsgitoff").
        KeyCode::Char(c) if c.is_ascii_graphic() || c == ' ' => app.palette_push(c),
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

/// Approval cursor-navigation helpers. Split out so this file stays under
/// the file-size gate.
mod approval;
use approval::{approval_next, approval_prev, option_count};

/// AskUserQuestion card key handlers, split out to keep this module under
/// the file-size gate.
mod ask_question;
mod trajectory;
mod worktree_pane;
use ask_question::handle_ask_question;

/// Tool-approval prompt keys: navigate three options, confirm, or dismiss.
/// The prompt renders inline at the transcript tail (see working::draw_transcript).
/// One-at-a-time: Enter sends a single approve/reject decision for the
/// current call_id; Esc rejects only the current one. The core applies the
/// single decision and re-interrupts for the next pending approval, so the
/// next card appears automatically. This is not a reject-all flow.
fn handle_approval(app: &mut App, k: KeyEvent) {
    let Some(a) = app.approval.as_mut() else {
        return;
    };
    match k.code {
        // 1=Yes, 2=Yes-don't-ask, 3=No. a/r pin approve/reject. When remember
        // is hidden (a protected-path ask), '2' selects No and '3' is a no-op
        // so the user cannot reach a choice the gate will ignore.
        KeyCode::Char('1') | KeyCode::Char('a') => a.selected = 0,
        KeyCode::Char('2') => a.selected = if a.remember_hidden() { 1 } else { 2 },
        KeyCode::Char('3') | KeyCode::Char('r') => {
            if !a.remember_hidden() {
                a.selected = 1;
            }
        }
        // Cyclic navigation through display order (Yes -> don't-ask -> No).
        KeyCode::Up | KeyCode::Left | KeyCode::Char('h') => {
            a.selected = approval_prev(a.selected, option_count(a));
        }
        KeyCode::Down | KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('d') => {
            a.selected = approval_next(a.selected, option_count(a));
        }
        KeyCode::Enter => {
            // Capture the focused verdict by identity before the mutable
            // resolve so the next popup for this tool preselects it.
            let (call_id, tool, kind, approved, persist) = {
                let a = app.approval.as_ref().expect("approval present");
                (
                    a.call_id.clone(),
                    a.tool.clone(),
                    a.focused_kind(),
                    a.focused_approves(),
                    a.focused_persists(),
                )
            };
            app.sticky_choices.insert(tool, kind);
            if app.session.is_some() {
                // Ship the verdict over the wire as a reverse response; the
                // server drives runner.resume, records the audit event, and —
                // when scope is "always" — computes the bash command prefix
                // from the approval's own tool+input and applies a scoped
                // always-allow rule to the gate. The prefix scoping lives
                // server-side so the TUI never imports the permission crate.
                let scope = if persist { "always" } else { "once" }.to_string();
                app.resolve_current_approval(
                    houyicoder_protocol::frontend::run::ApprovalDecision {
                        call_id,
                        approved,
                        updated_input: None,
                        scope,
                    },
                );
            } else {
                app.approval = None;
            }
        }
        KeyCode::Esc => {
            // Esc rejects ONLY the current approval — one reject verdict for
            // the current call_id. The server applies it and, if more
            // approvals remain, re-asks. This is not a reject-all. The reject
            // is recorded as a sticky reject-once so the next popup for this
            // tool preselects No.
            let (call_id, tool) = match app.approval.as_ref() {
                Some(a) => (a.call_id.clone(), a.tool.clone()),
                None => return,
            };
            app.sticky_choices.insert(
                tool,
                houyicoder_protocol::acp_wire::PermissionOptionKind::RejectOnce,
            );
            if app.session.is_some() {
                app.resolve_current_approval(
                    houyicoder_protocol::frontend::run::ApprovalDecision {
                        call_id,
                        approved: false,
                        updated_input: None,
                        scope: "once".to_string(),
                    },
                );
            } else {
                app.approval = None;
            }
        }
        _ => {}
    }
}

/// Input box keys: type, send, open palette, switch pane, toggle plan mode,
/// and per-pane hunk/finding keys.
fn handle_input(app: &mut App, k: KeyEvent) {
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
        // In the /status pane, Tab cycles the sub-tab (Status → Config → Usage)
        // instead of the whole pane (the pane owns its tab bar).
        KeyCode::Tab if app.pane == Pane::Status => app.status_tab = app.status_tab.next(),
        // On the /status Status tab, the e key opens the inline session-name
        // editor (houyi makes the session name
        // inline-editable rather than a rename command). Gated on the Status tab + an empty input box so
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
        // Shift+Tab cycles the permission mode: default → auto → bypass →
        // default. No pane shadows it now (the /memory scope filter moved to
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
        // key must actually close it — without this arm Esc was a dead key
        // when the input box + the search filter were both empty. Matches
        // the Artifact pane's Esc-to-transcript behavior.
        KeyCode::Esc if app.pane == Pane::Memory => {
            app.pane = Pane::Transcript;
        }
        // /worktrees pane: delegated to keys/worktree_pane.rs.
        _ if app.pane == Pane::Worktree && worktree_pane::handle(app, k) => {}
        // /trajectory pane: delegated to keys/trajectory.rs.
        _ if app.pane == Pane::Trajectory && trajectory::handle(app, k) => {}
        // /status pane: Left/Right (or Tab) cycle the sub-tab (Status →
        // Config → Usage), Esc dismisses back to the transcript. A
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
        KeyCode::Esc if app.pane == Pane::Hooks => {
            if app.hooks_level.get() > 0 {
                app.hooks_level.set(0);
            } else {
                app.pane = Pane::Transcript;
            }
        }
        KeyCode::Esc if app.pane == Pane::Skills => {
            app.pane = Pane::Transcript;
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
        // '/' opens the slash palette only when no typed permission sub-mode
        // (add rule / add directory) is active — an AddDir path typically
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
        // not the whole text — double-press Home walks up wrapped lines.
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
fn cycle_pane(app: &mut App) {
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

#[cfg(test)]
#[path = "keys_tests.rs"]
mod tests;
