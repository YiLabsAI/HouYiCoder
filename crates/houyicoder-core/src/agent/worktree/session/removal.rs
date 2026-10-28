//! Taking a managed worktree apart: the checks that establish the object, the
//! Git steps that remove it, and what a removal that stops halfway leaves for
//! the next attempt.

use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

use super::super::cleanup::PendingCleanup;
use super::super::cleanup::clear_pending;
use super::super::cleanup::lookup_pending;
use super::super::cleanup::remove_tree;
use super::super::cleanup::write_pending;
use super::super::identify::TargetStamp;
use super::super::identify::listed_in_git;
use super::super::identify::resolve_identity;
use super::super::preview::MergeState;
use super::super::preview::merge_state;
use super::super::record::ManagedWorktree;
use super::super::record::RecordLookup;
use super::super::record::WorktreeLease;
use super::super::record::clear_managed_state;
use super::super::record::lookup_record;
use super::WorktreeError;
use super::canonicalize;
use super::run_git;
use super::run_git_code;
use super::worktree_path_for;
use super::worktrees_dir;

/// Remove a linked worktree + delete its branch (the exit remove path). The
/// user has already accepted the loss, so Git's refusal to remove a tree with
/// uncommitted work is overridden once; a worktree Git locked still refuses.
/// A slug without a record this version reads, or a directory that is not the
/// tree that record names, is refused before Git is asked.
pub fn remove_worktree(repo_root: &Path, slug: &str) -> Result<(), WorktreeError> {
    let repo_root = canonicalize(repo_root)?;
    remove_linked(&repo_root, slug, RemovalForce::AuthorizedDiscard)
}

/// Remove a linked worktree + delete its branch, conservatively, for the
/// child auto-reclaim path where the caller has only proven the tree clean
/// rather than obtained an explicit discard. Git's refusals stand, branch -d
/// refuses to delete work that is not merged, and the same record and identity
/// checks as the exit path run before anything is removed.
pub fn remove_clean_worktree(repo_root: &Path, slug: &str) -> Result<(), WorktreeError> {
    let repo_root = canonicalize(repo_root)?;
    remove_linked(&repo_root, slug, RemovalForce::Conservative)
}

/// How much of Git's own caution a removal is allowed to override.
#[derive(Clone, Copy)]
enum RemovalForce {
    /// Remove only what Git removes on its own, and delete the branch only
    /// when its work is merged.
    Conservative,
    /// The user accepted the loss: a tree with uncommitted work is removed,
    /// and an unmerged branch is deleted. A worktree Git locked still refuses.
    AuthorizedDiscard,
}

impl RemovalForce {
    fn remove_args(self) -> Vec<&'static str> {
        match self {
            Self::Conservative => vec!["worktree", "remove"],
            Self::AuthorizedDiscard => vec!["worktree", "remove", "--force"],
        }
    }
}

/// Remove the directory Git registered for a slug, then delete its branch,
/// with the lease held for the whole act.
///
/// The lease is taken rather than asked about, so a session that acquires it
/// after a probe cannot begin working in a tree this call is deleting. Inside
/// it the act is verified before anything is removed: the slug has to carry a
/// record this version reads, the directory has to resolve to the tree that
/// record names, and Git must not have it locked. A directory at the managed
/// path without that evidence is left alone whatever Git has registered there.
///
/// Each step runs only after the one before it is confirmed, and the record is
/// cleared last. A failure before the directory is gone leaves the directory,
/// whatever it still holds, its branch and its record. A branch Git refuses to
/// delete once the directory is gone answers RemovalIncomplete, naming what
/// survived instead of a Git message about a step that never ran, and the
/// record of the target lets the next attempt finish only that branch.
fn remove_linked(repo_root: &Path, slug: &str, force: RemovalForce) -> Result<(), WorktreeError> {
    let lease = WorktreeLease::acquire(repo_root, slug)?;
    match remove_verified(repo_root, slug, force) {
        Ok(()) => {
            // The lease is still held, so the lease file this deletes is one no
            // other process can hold.
            clear_managed_state(repo_root, slug);
            Ok(())
        }
        Err(error) => {
            lease.discard();
            Err(error)
        }
    }
}

/// The verified part of a removal: the checks that establish the object, then
/// the Git steps that take it apart.
fn remove_verified(repo_root: &Path, slug: &str, force: RemovalForce) -> Result<(), WorktreeError> {
    let record = match lookup_record(repo_root, slug) {
        RecordLookup::Read(record) => record,
        RecordLookup::Rejected { reason } => {
            return Err(WorktreeError::RecordRejected {
                slug: slug.to_string(),
                reason,
            });
        }
        RecordLookup::Absent => {
            return Err(WorktreeError::Unmanaged {
                slug: slug.to_string(),
            });
        }
    };
    // A removal that stopped once Git had detached the registration left a
    // record, and this attempt finishes that one target. When Git names the
    // directory as a worktree again, the record describes a leftover that is
    // no longer there and the ordinary path below acts on the tree.
    if let Some(pending) = lookup_pending(repo_root, slug)? {
        let repo = canonicalize(repo_root)?;
        if !listed_in_git(&repo, &worktree_path_for(&repo, slug))? {
            return finish_pending(&repo, slug, &record, &pending, force);
        }
        clear_pending(repo_root, slug);
    }
    // The numbers naming the directory are read before the checks rather than
    // after them, and compared again right before Git is asked. Read after,
    // they would only describe whatever sits there by then, which is the one
    // thing the checks did not agree to.
    let stamp = TargetStamp::read(&worktree_path_for(repo_root, slug))?;
    let identity = resolve_identity(repo_root, slug, record.branch())?;
    // Removal already refuses a locked tree through Git's own caution. Saying
    // so here names the reason instead of leaving a Git message about a
    // worktree that is locked.
    if identity.locked {
        return Err(WorktreeError::Locked {
            path: identity.path,
            reason: "Git records a lock on it".to_string(),
        });
    }
    let path = identity.path;
    let path_text = path.to_string_lossy().into_owned();
    let mut args: Vec<&str> = force.remove_args();
    args.push(&path_text);
    if let Some(stamp) = stamp
        && let Err(reason) = stamp.verify(&path)
    {
        return Err(WorktreeError::Identity { path, reason });
    }
    // The record of the target goes down before Git is asked to take the
    // registration apart, so a removal that stops, or a process that dies,
    // between the two halves leaves a target the next attempt can finish.
    // While Git still lists the tree the record is inert: the next attempt
    // clears it and takes the ordinary path. A record that cannot be stored
    // stops the removal here, while the tree is still whole: past this point
    // the registration can be detached, and a directory left without a record
    // is one no later attempt knows how to finish.
    write_pending(repo_root, &PendingCleanup::new(slug, &path, stamp))?;
    run_git(repo_root, &args)?;
    // A stat error is not the directory being gone: the branch is deleted and
    // the record cleared only once the directory is proven absent, so a
    // directory that cannot be checked keeps everything.
    match path.try_exists() {
        Ok(false) => {}
        Ok(true) => {
            // Git detached the registration and left the directory. The record
            // names the target a later attempt may finish; this attempt
            // finishes it now. The walk compares the numbers read at the start
            // again before it deletes anything, so a directory replaced in
            // between is reported rather than deleted.
            if let Err(reason) = remove_tree(&path, stamp) {
                return Err(WorktreeError::CleanupPending { path, reason });
            }
        }
        Err(error) => {
            return Err(WorktreeError::CleanupPending {
                path,
                reason: format!(
                    "git detached its registration, and whether the directory is gone could not be checked: {error}"
                ),
            });
        }
    }
    delete_branch(repo_root, &record, force, path)
}

/// Finish a removal Git left half done, under the lease the caller holds.
///
/// Only the directory the record names is finished. The slug is resolved again
/// rather than trusted from the record, and the two have to name the same
/// directory; a leftover that moved, or that sits outside the managed root, is
/// refused rather than replaced by whatever the slug maps to today. The numbers
/// the record carries have to still describe the directory there, so a
/// directory replaced since the record was written is reported instead of
/// deleted. A directory already gone means only the branch and the records are
/// left, which is what a previous attempt stopped before.
fn finish_pending(
    repo_root: &Path,
    slug: &str,
    record: &ManagedWorktree,
    pending: &PendingCleanup,
    force: RemovalForce,
) -> Result<(), WorktreeError> {
    if pending.slug() != slug {
        return Err(WorktreeError::CleanupPending {
            path: pending.path().to_path_buf(),
            reason: format!("the record names worktree {}", pending.slug()),
        });
    }
    let expected = worktree_path_for(repo_root, slug);
    let resolved = match dunce::canonicalize(&expected) {
        Ok(path) => path,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return delete_branch(repo_root, record, force, expected);
        }
        Err(error) => {
            return Err(WorktreeError::CleanupPending {
                path: expected,
                reason: format!("it could not be resolved: {error}"),
            });
        }
    };
    let root = canonicalize(&worktrees_dir(repo_root))?;
    // The managed root is anchored to the repository here as it is when a tree
    // is entered: a root replaced by a link, combined with a record naming what
    // that link points at, would otherwise let this path delete outside the
    // repository.
    if !root.starts_with(repo_root) {
        return Err(WorktreeError::CleanupPending {
            path: root,
            reason: format!(
                "the managed directory resolves outside {}",
                repo_root.display()
            ),
        });
    }
    if resolved.parent() != Some(root.as_path()) {
        return Err(WorktreeError::CleanupPending {
            path: resolved,
            reason: format!("it resolves outside {}", root.display()),
        });
    }
    if resolved != pending.path() {
        return Err(WorktreeError::CleanupPending {
            path: resolved,
            reason: format!(
                "the record names {}, which is not the directory this slug resolves to",
                pending.path().display()
            ),
        });
    }
    if let Err(reason) = remove_tree(&resolved, pending.stamp()) {
        return Err(WorktreeError::CleanupPending {
            path: resolved,
            reason,
        });
    }
    delete_branch(repo_root, record, force, resolved)
}

/// Delete the worktree branch once its directory is gone.
///
/// A branch proven merged into the branch the tree was forked from is deleted
/// with the flag that keeps Git's refusal for a branch holding unmerged work.
/// Only an authorized discard may use the flag that overrides it, and a merged
/// branch still falls back to that flag when Git refuses because the repository
/// is checked out where this work is not: the work is in the base branch
/// either way, so the branch name is all that is left to delete.
///
/// A branch that is already gone is the step being done, not a failure: an
/// attempt that deleted it and then died before clearing the record leaves the
/// next attempt with nothing left to delete.
fn delete_branch(
    repo_root: &Path,
    record: &ManagedWorktree,
    force: RemovalForce,
    path: PathBuf,
) -> Result<(), WorktreeError> {
    let branch = record.branch().to_string();
    let reference = format!("refs/heads/{branch}");
    match run_git_code(repo_root, &["rev-parse", "--verify", "--quiet", &reference]) {
        // Only the quiet probe's answer for a revision it cannot resolve means
        // the branch is gone. Any other status, or a run that never finished,
        // is a question Git did not answer: a branch this removal could not
        // confirm is reported, not left behind under a record already cleared.
        Some(1) => return Ok(()),
        Some(0) => {}
        Some(status) => {
            return Err(WorktreeError::RemovalIncomplete {
                path,
                branch,
                reason: format!("git could not say whether the branch is there: status {status}"),
            });
        }
        None => {
            return Err(WorktreeError::RemovalIncomplete {
                path,
                branch,
                reason: "git did not finish saying whether the branch is there".to_string(),
            });
        }
    }
    let merged = matches!(
        merge_state(repo_root, record),
        Ok(MergeState::Merged { .. })
    );
    let discard_allowed = matches!(force, RemovalForce::AuthorizedDiscard);
    let first = if merged || !discard_allowed {
        "-d"
    } else {
        "-D"
    };
    match run_git(repo_root, &["branch", first, &branch]) {
        Ok(_) => Ok(()),
        Err(error) => {
            if discard_allowed && merged && run_git(repo_root, &["branch", "-D", &branch]).is_ok() {
                return Ok(());
            }
            Err(WorktreeError::RemovalIncomplete {
                path,
                branch,
                reason: error.to_string(),
            })
        }
    }
}
