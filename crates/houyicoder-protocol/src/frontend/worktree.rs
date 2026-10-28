//! Managed-worktree replies the host sends to the terminal.
//!
//! These carry decisions rather than model turns. A preview names exactly what
//! a removal would take away, and the token it carries names that same state,
//! so the removal that follows acts only on what the user was shown. A token
//! the host no longer finds matching answers nothing about what changed: the
//! request has to be previewed again.

use serde::{Deserialize, Serialize};

/// What a removal preview found in a managed worktree.
///
/// Every count is a number the host measured, so a count of zero means the
/// host looked and found none. A question the host could not answer is None,
/// never a zero standing in for an unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreePreviewView {
    /// The name the host records the worktree under. Requests name the
    /// directory instead; this is what the pane shows the user.
    pub slug: String,
    /// The directory a removal would take apart.
    pub path: String,
    /// The branch the worktree is checked out on.
    pub branch: String,
    /// The commit the worktree has checked out.
    pub head: String,
    /// Tracked files with staged or unstaged edits.
    pub tracked_dirty: u64,
    /// Paths Git does not track and an ignore rule does not cover.
    pub untracked: u64,
    /// Ignored directories a build-output name covers, as Git listed them.
    pub ignored_build_dirs: Vec<String>,
    /// Ignored entries no build-output name covers.
    pub ignored_unknown: u64,
    /// Whether the worktree branch is merged into the branch it was forked
    /// from. False when the host could not answer, so this alone does not say
    /// the branch holds no work: a reader that needs to know whether work
    /// would be lost reads unmerged_commits, which counts the commits ahead
    /// for a branch Git measured and is None for one it did not answer about.
    pub merged: bool,
    /// The branch the merge question was asked against. None when the creation
    /// record named no base branch.
    pub base_branch: Option<String>,
    /// Commits the base branch does not contain. None when no base branch was
    /// recorded or Git could not count them; reading that as zero would
    /// report work the removal would discard as work that does not exist.
    pub unmerged_commits: Option<u64>,
    /// Whether a live process holds the worktree's active-use lease.
    pub leased: bool,
    /// Whether Git records the worktree as locked, which refuses removal
    /// until the lock is released.
    pub locked: bool,
    /// A digest of the tree's state, to be sent back with the removal. It
    /// proves nothing on its own: a removal compares it against the state it
    /// reads, so the comparison is what holds, not the value.
    pub token: String,
}

/// The worktree a session entered, as the terminal reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeEnterView {
    /// The directory the session now works in.
    pub path: String,
    /// The branch the worktree is checked out on.
    pub branch: String,
}

/// How a confirmed removal ended.
///
/// The last two variants are not failures of the same weight: Incomplete means
/// the directory is gone and its branch survived, Pending means the directory
/// survived. Both keep the record a later attempt finishes the removal from.
/// A terminal reports which of the two ended and what survived; whether it can
/// ask for that later attempt depends on the directory still being a worktree
/// the host can read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum WorktreeRemoveOutcome {
    /// The directory and its branch are gone.
    Removed,
    /// Nothing was removed: the worktree no longer matched the preview the
    /// user confirmed, so it has to be previewed again before another attempt.
    Stale,
    /// The directory is gone and the branch survives.
    Incomplete {
        /// The directory that was taken apart.
        path: String,
        /// The branch that still exists.
        branch: String,
        /// What stopped the branch deletion.
        reason: String,
    },
    /// The directory survives, recorded so a later attempt finishes it.
    Pending {
        /// The directory that survived.
        path: String,
        /// What stopped the removal.
        reason: String,
    },
}
