//! Map the host's worktree decisions to the forms the terminal reads, so the
//! pane renders a preview or a removal result without importing the engine
//! crate.
//!
//! The mapping copies what the host measured and never fills a gap: a merge
//! answer the host could not give stays absent, and a removal that stopped
//! partway keeps the parts that survived instead of reporting a clean result.

use houyicoder_core::agent::worktree::preview::MergeState;
use houyicoder_core::agent::worktree::preview::WorktreePreview;
use houyicoder_core::agent::worktree::session::WorktreeError;
use houyicoder_protocol::frontend::worktree::WorktreeEnterView;
use houyicoder_protocol::frontend::worktree::WorktreePreviewView;
use houyicoder_protocol::frontend::worktree::WorktreeRemoveOutcome;

/// Map a preview to the form the pane reads.
pub(crate) fn map_preview(slug: &str, preview: WorktreePreview) -> WorktreePreviewView {
    let (merged, base_branch) = match &preview.merge {
        MergeState::Merged { base } => (true, Some(base.clone())),
        MergeState::NotMerged { base } => (false, Some(base.clone())),
        MergeState::Unknown { base } => (false, base.clone()),
    };
    WorktreePreviewView {
        slug: slug.to_string(),
        path: preview.identity.path.to_string_lossy().into_owned(),
        branch: preview.identity.branch,
        head: preview.identity.head,
        tracked_dirty: preview.changes.tracked_dirty as u64,
        untracked: preview.changes.untracked as u64,
        ignored_build_dirs: preview.changes.ignored_build_dirs,
        ignored_unknown: preview.changes.ignored_unknown as u64,
        merged,
        base_branch,
        unmerged_commits: preview.unmerged_commits.map(u64::from),
        leased: preview.leased,
        locked: preview.locked,
        token: preview.token,
    }
}

/// Map an entered worktree to the form the pane reads.
pub(crate) fn map_entered(path: String, branch: String) -> WorktreeEnterView {
    WorktreeEnterView { path, branch }
}

/// Map a removal that ran, or the error that stopped it.
///
/// The three unfinished shapes answer as outcomes, because each one names what
/// survived rather than reporting a clean result. Every other error is
/// returned for the caller to report: those are refusals that changed nothing.
pub(crate) fn map_removal(
    result: Result<(), WorktreeError>,
) -> Result<WorktreeRemoveOutcome, WorktreeError> {
    match result {
        Ok(()) => Ok(WorktreeRemoveOutcome::Removed),
        Err(WorktreeError::Stale { .. }) => Ok(WorktreeRemoveOutcome::Stale),
        Err(WorktreeError::RemovalIncomplete {
            path,
            branch,
            reason,
        }) => Ok(WorktreeRemoveOutcome::Incomplete {
            path: path.to_string_lossy().into_owned(),
            branch,
            reason,
        }),
        Err(WorktreeError::CleanupPending { path, reason }) => Ok(WorktreeRemoveOutcome::Pending {
            path: path.to_string_lossy().into_owned(),
            reason,
        }),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests;
