//! Pane-state predicate helpers for the key handlers, split from keys.rs so
//! that file stays under the file-size gate. Each predicate is a pure read
//! over App state; the key handlers gate shortcut arms on them so an
//! in-progress artifact edit is not derailed and an action only fires in the
//! pane + stage where it is meaningful.

use crate::state::{App, Pane, Stage};

/// True when the current pane has a focusable list (Up/Down do something).
pub(crate) fn pane_navigable(app: &App) -> bool {
    matches!(
        app.pane,
        Pane::Diff | Pane::Review | Pane::Spec | Pane::Artifact
    )
}

/// Panes that own Esc for their own close or back navigation: Esc there must
/// close or back the pane, not reach the run interrupt or the queue recall.
/// A single predicate shared by both Esc arms so the two cannot drift apart
/// (a copied list is how Pane::Trajectory fell out of one arm and stayed in
/// none).
pub(crate) fn pane_owns_esc(pane: Pane) -> bool {
    pane_replaces_input(pane) || pane == Pane::Artifact
}

/// The panes that stand in for the interaction surface: the input box and the
/// status row retract, Esc closes them, and the typing keys must not reach a
/// box that is not on screen.
///
/// One list, because six hand-copied copies of it drifted apart three times.
/// The failures were not symmetric: a pane missing from the Esc list had its
/// key stolen by the abort arm, so Esc interrupted the running agent; a pane
/// missing from the typing lists took characters into a hidden box, and the
/// resulting non-empty input silently disabled the pane's own Enter. Adding a
/// pane to one list and not the others is how both were built.
///
/// The artifact surface is deliberately not here: it owns Esc through its own
/// multi-level handler but keeps the input box, since its edit mode types
/// into it.
pub(crate) fn pane_replaces_input(pane: Pane) -> bool {
    matches!(
        pane,
        Pane::Model
            | Pane::Hooks
            | Pane::Status
            | Pane::Memory
            | Pane::Worktree
            | Pane::Trajectory
            | Pane::Resume
            | Pane::Skills
            | Pane::Agents
    )
}

/// True when the current pane + stage accepts an approve action. The artifact
/// pane approves a pending proposed edit whenever one exists (stage-independent).
pub(crate) fn pane_approvable(app: &App) -> bool {
    matches!(
        (app.pane, app.stage),
        (Pane::Spec | Pane::Plan, Stage::Design)
            | (Pane::Diff, Stage::Implementing)
            | (Pane::Review | Pane::Verify, Stage::Verify)
    ) || (app.pane == Pane::Artifact && app.artifact.pending_proposal().is_some())
}

/// True when the current pane + stage accepts a reject action. The artifact
/// pane rejects a pending proposed edit whenever one exists.
pub(crate) fn pane_rejectable(app: &App) -> bool {
    matches!(
        (app.pane, app.stage),
        (Pane::Diff, Stage::Implementing) | (Pane::Review, Stage::Verify)
    ) || (app.pane == Pane::Artifact && app.artifact.pending_proposal().is_some())
}

/// True when the current pane + stage accepts a rework action (backward path
/// to implementing).
pub(crate) fn pane_reworkable(app: &App) -> bool {
    matches!(
        (app.pane, app.stage),
        (Pane::Review | Pane::Verify, Stage::Verify)
    )
}

/// True when the artifact pane is in an edit mode (Replace/Insert/NaturalLanguage).
/// While editing, single-char shortcuts (q to quit, g to replay, / for palette,
/// Tab to cycle pane, PageUp/End to scroll) are suppressed so the user can type
/// edit text without quitting or fleeing the pane. Only Esc, Enter, Backspace,
/// and printable chars act.
pub(crate) fn artifact_editing(app: &App) -> bool {
    app.pane == Pane::Artifact && !app.artifact.mode().is_normal()
}
