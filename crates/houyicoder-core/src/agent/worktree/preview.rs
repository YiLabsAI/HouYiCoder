//! What removing a worktree would discard: the identity the directory must
//! still have, the changes it holds split by class, whether its branch is
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
use super::identify::WorktreeRole;
use super::identify::WorktreeTarget;
use super::identify::fnv1a;
use super::identify::resolve_identity;
use super::record::ManagedWorktree;
use super::record::RecordLookup;
use super::record::leased_for;
use super::record::lookup_record;
use super::record::worktree_leased;
use super::session::WorktreeError;
use super::session::canonicalize;
use super::session::current_branch;
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
    Unknown {
        /// The base the question was asked against, when the creation record
        /// named one. A caller can then say which question went unanswered
        /// instead of reporting that no base exists.
        base: Option<String>,
    },
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
    /// Whether the removal may delete the worktree's branch. A tree this
    /// product created owns the branch it made, and a tree entered from
    /// elsewhere does not: its branch belongs to whoever made the tree, so a
    /// removal takes the directory and leaves the branch.
    pub branch_deletable: bool,
    /// A digest of the tree's own state: the identity, the changes, and how
    /// the branch stands. It proves nothing on its own, and a caller that
    /// trusts it without re-reading the tree has a digest of whatever it was
    /// shown, not a permission. The lease is not part of it: a removal holds
    /// the lease while it compares, and a tree another process has leased
    /// refuses at that acquisition instead.
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
    let token = preview_token(&identity, &changes, &merge, unmerged_commits, locked);
    Ok(WorktreePreview {
        identity,
        changes,
        merge,
        unmerged_commits,
        leased,
        locked,
        branch_deletable: true,
        token,
    })
}

/// Preview removing a resolved worktree, whoever created it.
///
/// A tree this product created goes through the creation record, which is the
/// evidence that the directory still is the object the record describes. Any
/// other tree has no record, so what it is comes from Git's own worktree
/// listing alone, and the preview says what it can measure there: its changes,
/// its lease, and its lock. No base branch was recorded for it, so the merge
/// question is left unanswered rather than answered against a guess, and its
/// branch is not the removal's to delete.
///
/// The main working tree is refused: a preview of taking it apart would offer
/// the user a removal that could never run.
pub fn preview_target(
    repo_root: &Path,
    target: &WorktreeTarget,
) -> Result<WorktreePreview, WorktreeError> {
    match &target.role {
        WorktreeRole::Linked { slug: Some(slug) } => preview_remove_worktree(repo_root, slug),
        WorktreeRole::MainCheckout => Err(WorktreeError::MainCheckout {
            path: target.path().to_path_buf(),
        }),
        WorktreeRole::Linked { slug: None } => preview_foreign(repo_root, target),
    }
}

/// The preview of a tree this product did not create. The lease is read before
/// the tree is, for the same reason the managed preview does it: a lease that
/// cannot be read refuses the preview, and the status read behind it is then
/// not paid for.
pub(super) fn preview_foreign(
    repo_root: &Path,
    target: &WorktreeTarget,
) -> Result<WorktreePreview, WorktreeError> {
    let repo_root = canonicalize(repo_root)?;
    let leased = leased_for(&repo_root, target)?;
    let changes = read_changes(target.path())?;
    // No creation record exists, so the branch to compare against is the one
    // the main working tree is on: that is the branch this tree would be
    // merged into. The question is the same one a managed tree asks, asked
    // against the base Git itself reports instead of one a record names.
    let base = current_branch(&repo_root);
    let merge = foreign_merge_state(&repo_root, &target.identity.branch, base.as_deref());
    let unmerged_commits = count_foreign_unmerged(&repo_root, &target.identity.branch, &merge);
    let locked = target.identity.locked;
    let token = preview_token(&target.identity, &changes, &merge, unmerged_commits, locked);
    Ok(WorktreePreview {
        identity: target.identity.clone(),
        changes,
        merge,
        unmerged_commits,
        leased,
        locked,
        branch_deletable: false,
        token,
    })
}

/// How an unmanaged worktree's branch stands against the branch the main
/// working tree is on.
///
/// The answer comes from Git's own ancestry question, so it holds whatever
/// created the directory. Detached HEAD in the main working tree leaves no
/// branch to ask about, and that is reported as an unknown base with no name
/// rather than an answer invented against some other ref.
fn foreign_merge_state(repo_root: &Path, branch: &str, base: Option<&str>) -> MergeState {
    let Some(base) = base else {
        return MergeState::Unknown { base: None };
    };
    let head = format!("refs/heads/{branch}");
    let base_ref = format!("refs/heads/{base}");
    match run_git_code(
        repo_root,
        &["merge-base", "--is-ancestor", &head, &base_ref],
    ) {
        Some(0) => MergeState::Merged {
            base: base.to_string(),
        },
        Some(1) => MergeState::NotMerged {
            base: base.to_string(),
        },
        // Git answered neither way: the branch may be gone, or the base may
        // not resolve. The base name is kept so the pane can say which
        // question went unanswered.
        _ => MergeState::Unknown {
            base: Some(base.to_string()),
        },
    }
}

/// How many commits the base branch does not contain. The count is what the
/// user sees beside the merge state; the state itself comes from the ancestry
/// question, never from this number.
fn count_foreign_unmerged(repo_root: &Path, branch: &str, merge: &MergeState) -> Option<u32> {
    let base = match merge {
        MergeState::Merged { base } | MergeState::NotMerged { base } => base,
        MergeState::Unknown { .. } => return None,
    };
    let head = format!("refs/heads/{branch}");
    let base_ref = format!("refs/heads/{base}");
    let counted = run_git(
        repo_root,
        &["rev-list", "--count", &format!("{base_ref}..{head}")],
    )
    .ok()?;
    counted.trim().parse().ok()
}

/// Whether the worktree branch's commits are all reachable from the branch it
/// was forked from. Git answers with its exit status: zero means merged, one
/// means unmerged, and any other ending means the question could not be asked.
pub(super) fn merge_state(
    repo_root: &Path,
    record: &ManagedWorktree,
) -> Result<MergeState, WorktreeError> {
    let Some(base) = record.base_branch() else {
        return Ok(MergeState::Unknown { base: None });
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
        _ => Ok(MergeState::Unknown {
            base: Some(base.to_string()),
        }),
    }
}

/// How many commits the base branch does not contain. The count is what the
/// user sees beside the merge state; the state itself comes from the ancestry
/// question, never from this number.
fn count_unmerged(repo_root: &Path, record: &ManagedWorktree, merge: &MergeState) -> Option<u32> {
    if matches!(merge, MergeState::Unknown { .. }) {
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
/// produce the same token, and every fact about the tree the preview reports
/// is part of it: a moved head, another change count, another merge answer or
/// unmerged count, or a lock each produce a different one. The lease is left
/// out on purpose, and the field list below says why.
fn preview_token(
    identity: &WorktreeIdentity,
    changes: &WorktreeChanges,
    merge: &MergeState,
    unmerged_commits: Option<u32>,
    locked: bool,
) -> String {
    let merge_label = match merge {
        MergeState::Merged { base } => format!("merged:{base}"),
        MergeState::NotMerged { base } => format!("unmerged:{base}"),
        MergeState::Unknown { base } => match base {
            Some(base) => format!("unknown:{base}"),
            None => "unknown:".to_string(),
        },
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
        // The lease is left out on purpose. The removal takes the lease before
        // it compares this digest, so its own hold would make every comparison
        // fail; a tree another process has leased already refuses at that
        // acquisition, which is the same question asked where it can be
        // answered.
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

#[cfg(test)]
mod tests;
