//! What removing a managed worktree would discard: the identity the directory
//! must still have, the changes it holds split by class, whether its branch is
//! merged, and a token naming the state the user was shown.
//!
//! The preview is evidence, not permission. The token names exactly the state
//! the preview reported, so a later preview of the same tree can be compared
//! with it. It is not a hash of file contents: an edit to a file already
//! counted as changed does not move it.

use std::path::Path;

use super::changes::WorktreeChanges;
use super::changes::read_changes;
use super::identify::WorktreeIdentity;
use super::identify::resolve_identity;
use super::record::ManagedWorktree;
use super::record::RecordLookup;
use super::record::lookup_record;
use super::record::worktree_leased;
use super::session::WorktreeError;
use super::session::canonicalize;
use super::session::run_git;
use super::session::run_git_code;
use super::session::validate_worktree_slug;

/// How a worktree's branch stands against the branch it was forked from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeState {
    /// Every commit on the worktree branch is reachable from the base branch.
    Merged {
        /// The branch the question was asked against.
        base: String,
    },
    /// The base branch does not contain every commit on the worktree branch.
    NotMerged {
        /// The branch the question was asked against.
        base: String,
    },
    /// No base branch was recorded, or Git could not answer the question.
    Unknown,
}

/// What removing a worktree would discard, and the state a decision was made
/// about.
#[derive(Clone, Debug)]
pub struct WorktreePreview {
    /// The worktree the removal would act on.
    pub identity: WorktreeIdentity,
    /// The changes it holds, by class.
    pub changes: WorktreeChanges,
    /// How its branch stands against the branch it was forked from.
    pub merge: MergeState,
    /// Commits on the worktree branch the base branch does not contain, so a
    /// removal shows how much work it would discard. None when no base branch
    /// was recorded or Git could not count them; a removal must not read that
    /// as zero.
    pub unmerged_commits: Option<u32>,
    /// Whether a live process holds the worktree's active-use lease.
    pub leased: bool,
    /// Whether Git records the worktree as locked, which refuses removal until
    /// the lock is cleared or a second force is given.
    pub locked: bool,
    /// A digest of every field above, used only to detect that the state moved
    /// between the preview and the act. It proves nothing on its own: a caller
    /// that trusts it without re-running the preview has a digest of whatever
    /// it was shown, not a permission.
    pub token: String,
}

/// Preview removing the worktree created under a slug.
///
/// Nothing is written and nothing is deleted. An error means the directory is
/// not a managed worktree in the state its creation record describes, so a
/// caller must not remove it: no record, a record this version refuses, a path
/// that resolves outside the managed directory, a Git common directory that
/// belongs to another repository, a worktree checked out on a branch it was
/// not created with, and a directory Git does not name in its own worktree
/// record all answer as errors rather than as a preview of an unknown object.
/// A slug outside the allowlist is one of those errors: it names no worktree
/// this product could have created.
pub fn preview_remove_worktree(
    repo_root: &Path,
    slug: &str,
) -> Result<WorktreePreview, WorktreeError> {
    validate_worktree_slug(slug)?;
    let record = match lookup_record(repo_root, slug) {
        RecordLookup::Read(record) => record,
        RecordLookup::Absent => {
            return Err(WorktreeError::Unmanaged {
                slug: slug.to_string(),
            });
        }
        RecordLookup::Rejected { reason } => {
            return Err(WorktreeError::RecordRejected {
                slug: slug.to_string(),
                reason,
            });
        }
    };
    let repo_root = canonicalize(repo_root)?;
    let identity = resolve_identity(&repo_root, slug, record.branch())?;
    // The lease is probed before the tree is read: a lease that cannot be
    // read refuses the preview, and the status read it would have preceded is
    // then not paid for.
    let leased = worktree_leased(&repo_root, slug)?;
    let changes = read_changes(&identity.path)?;
    let merge = merge_state(&repo_root, &record)?;
    let unmerged_commits = count_unmerged(&repo_root, &record, &merge);
    let locked = identity.locked;
    let token = preview_token(
        &identity,
        &changes,
        &merge,
        unmerged_commits,
        leased,
        locked,
    );
    Ok(WorktreePreview {
        identity,
        changes,
        merge,
        unmerged_commits,
        leased,
        locked,
        token,
    })
}

/// Whether the worktree branch's commits are all reachable from the branch it
/// was forked from. Git answers with its exit status: zero means merged, one
/// means unmerged, and any other ending means the question could not be asked.
pub(super) fn merge_state(
    repo_root: &Path,
    record: &ManagedWorktree,
) -> Result<MergeState, WorktreeError> {
    let Some(base) = record.base_branch() else {
        return Ok(MergeState::Unknown);
    };
    let branch = format!("refs/heads/{}", record.branch());
    let base_ref = format!("refs/heads/{base}");
    match run_git_code(
        repo_root,
        &["merge-base", "--is-ancestor", &branch, &base_ref],
    ) {
        Some(0) => Ok(MergeState::Merged {
            base: base.to_string(),
        }),
        Some(1) => Ok(MergeState::NotMerged {
            base: base.to_string(),
        }),
        _ => Ok(MergeState::Unknown),
    }
}

/// How many commits the base branch does not contain. The count is what the
/// user sees beside the merge state; the state itself comes from the ancestry
/// question, never from this number.
fn count_unmerged(repo_root: &Path, record: &ManagedWorktree, merge: &MergeState) -> Option<u32> {
    if *merge == MergeState::Unknown {
        return None;
    }
    let base = record.base_branch()?;
    let branch = format!("refs/heads/{}", record.branch());
    let base_ref = format!("refs/heads/{base}");
    let counted = run_git(
        repo_root,
        &["rev-list", "--count", &format!("{base_ref}..{branch}")],
    )
    .ok()?;
    counted.trim().parse().ok()
}

/// A digest of the previewed state. Two previews of an unchanged worktree
/// produce the same token, and every field the preview reports is part of it:
/// a moved head, another change count, another merge answer or unmerged
/// count, a lease, or a lock each produce a different one.
fn preview_token(
    identity: &WorktreeIdentity,
    changes: &WorktreeChanges,
    merge: &MergeState,
    unmerged_commits: Option<u32>,
    leased: bool,
    locked: bool,
) -> String {
    let merge_label = match merge {
        MergeState::Merged { base } => format!("merged:{base}"),
        MergeState::NotMerged { base } => format!("unmerged:{base}"),
        MergeState::Unknown => "unknown".to_string(),
    };
    // The two paths go in as their own bytes: a path that is not valid text
    // has no lossless string form, and a lossy one could make two directories
    // hash alike.
    let mut state: Vec<u8> = Vec::new();
    for path in [&identity.path, &identity.common_dir] {
        state.extend_from_slice(path.as_os_str().as_encoded_bytes());
        state.push(0);
    }
    let parts = [
        identity.branch.clone(),
        identity.head.clone(),
        changes.tracked_dirty.to_string(),
        changes.untracked.to_string(),
        changes.ignored_build_dirs.join("\0"),
        changes.ignored_unknown.to_string(),
        merge_label,
        match unmerged_commits {
            Some(count) => count.to_string(),
            None => "unknown".to_string(),
        },
        leased.to_string(),
        locked.to_string(),
    ];
    // A NUL cannot appear in a path or a branch name, so a value holding the
    // separator is impossible and two different states cannot produce the
    // same bytes by shifting content across a field boundary.
    for part in parts {
        state.extend_from_slice(part.as_bytes());
        state.push(0);
    }
    format!("{:016x}", fnv1a(&state))
}

/// FNV-1a over the previewed state. A change detector only needs to separate
/// two states of one worktree, so a short non-cryptographic digest is enough.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests;
