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
    matches!(
        pane,
        Pane::Memory
            | Pane::Artifact
            | Pane::Worktree
            | Pane::Status
            | Pane::Resume
            | Pane::Hooks
            | Pane::Model
            | Pane::Trajectory
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
