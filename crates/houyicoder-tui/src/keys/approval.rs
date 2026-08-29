//! Tool-approval key handling: cursor navigation helpers and the approval
//! prompt key handler. Split from the keys root so that module stays a thin
//! working-surface dispatcher. See keys::handle_working for the dispatch.

use crossterm::event::{KeyCode, KeyEvent};

use crate::state::App;

/// Display order of the three built-in approval options, the canonical
/// layout: Yes, then Yes-don't-ask, then No. The selected index
/// keeps its internal mapping (0=Yes, 1=No, 2=Yes-don't-ask); this array
/// maps display position to selected value. Used when the server sends no
/// options (the fallback built-in 3-option set).
pub(super) const APPROVAL_DISPLAY_ORDER: [usize; crate::records::APPROVAL_OPTIONS] = [0, 2, 1];

/// The number of options the cursor cycles through: the server's offered
/// list when present, otherwise the built-in set (two when remember is hidden
/// for a protected-path ask, three otherwise).
pub(super) fn option_count(a: &crate::records::Approval) -> usize {
    if a.options.is_empty() {
        a.visible_option_count()
    } else {
        a.options.len()
    }
}

/// Advance to the next option. Wraps around. When the server sends no
/// options, uses the built-in display order (Yes, Yes-don't-ask, No);
/// when it does, cycles linearly through the dynamic list.
pub(super) fn approval_next(current: usize, count: usize) -> usize {
    if count == crate::records::APPROVAL_OPTIONS {
        let pos = APPROVAL_DISPLAY_ORDER
            .iter()
            .position(|&s| s == current)
            .unwrap_or(0);
        APPROVAL_DISPLAY_ORDER[(pos + 1) % APPROVAL_DISPLAY_ORDER.len()]
    } else if count == 0 {
        0
    } else {
        (current + 1) % count
    }
}

/// Advance to the previous option. Wraps around.
pub(super) fn approval_prev(current: usize, count: usize) -> usize {
    if count == crate::records::APPROVAL_OPTIONS {
        let pos = APPROVAL_DISPLAY_ORDER
            .iter()
            .position(|&s| s == current)
            .unwrap_or(0);
        APPROVAL_DISPLAY_ORDER
            [(pos + APPROVAL_DISPLAY_ORDER.len() - 1) % APPROVAL_DISPLAY_ORDER.len()]
    } else if count == 0 {
        0
    } else {
        (current + count - 1) % count
    }
}

/// Tool-approval prompt keys: navigate three options, confirm, or dismiss.
/// The prompt renders inline at the transcript tail (see working::draw_transcript).
/// One-at-a-time: Enter sends a single approve/reject decision for the
/// current call_id; Esc rejects only the current one. The core applies the
/// single decision and re-interrupts for the next pending approval, so the
/// next card appears automatically. This is not a reject-all flow.
pub(super) fn handle_approval(app: &mut App, k: KeyEvent) {
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
                // server drives runner.resume, records the audit event, and,
                // when scope is "always", computes the bash command prefix
                // from the approval's own tool and input, and applies a scoped
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
            // Esc rejects ONLY the current approval; one reject verdict for
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
