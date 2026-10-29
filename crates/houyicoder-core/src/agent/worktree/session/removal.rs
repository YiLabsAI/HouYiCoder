//! Taking a managed worktree apart: the checks that establish the object, the
//! Git steps that remove it, and what a removal that stops halfway leaves for
//! the next attempt.

use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

use super::super::cleanup::PendingCleanup;
use super::super::cleanup::RemovalIntent;
use super::super::cleanup::clear_pending;
use super::super::cleanup::lookup_pending;
use super::super::cleanup::remove_tree;
use super::super::cleanup::write_pending;
use super::super::identify::TargetStamp;
use super::super::identify::WorktreeRole;
use super::super::identify::WorktreeTarget;
use super::super::identify::listed_in_git;
use super::super::identify::resolve_identity;
use super::super::preview::MergeState;
use super::super::preview::merge_state;
use super::super::preview::preview_foreign;
use super::super::preview::preview_remove_worktree;
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
    remove_linked(
        &repo_root,
        slug,
        RemovalForce::AuthorizedDiscard,
        Authorization::Discard,
    )
}

/// Remove a linked worktree + delete its branch, but only while the tree is
/// still the state its preview showed: the token a preview returned has to
/// name what the tree holds now.
///
/// The user confirmed that preview, so the removal has the same standing as an
/// accepted discard. The lease is taken before the token is compared, so
/// nothing can start working in the tree between the comparison and the act,
/// and a token that no longer matches removes nothing.
pub fn remove_previewed_worktree(
    repo_root: &Path,
    slug: &str,
    token: &str,
    delete_unmerged_branch: bool,
) -> Result<(), WorktreeError> {
    let repo_root = canonicalize(repo_root)?;
    let force = if delete_unmerged_branch {
        RemovalForce::AuthorizedDiscard
    } else {
        RemovalForce::AuthorizedKeepBranch
    };
    remove_linked(&repo_root, slug, force, Authorization::Previewed(token))
}

/// What entitles a caller to remove a worktree.
enum Authorization<'a> {
    /// The user accepted the loss through the exit discard gate.
    Discard,
    /// The user confirmed a preview, and this token names the state it showed.
    Previewed(&'a str),
}

/// Remove a linked worktree + delete its branch, conservatively, for the
/// child auto-reclaim path where the caller has only proven the tree clean
/// rather than obtained an explicit discard. Git's refusals stand, branch -d
/// refuses to delete work that is not merged, and the same record and identity
/// checks as the exit path run before anything is removed.
pub fn remove_clean_worktree(repo_root: &Path, slug: &str) -> Result<(), WorktreeError> {
    let repo_root = canonicalize(repo_root)?;
    remove_linked(
        &repo_root,
        slug,
        RemovalForce::Conservative,
        Authorization::Discard,
    )
}

/// Remove a worktree the user confirmed from the preview of a resolved
/// directory. A tree this product created is taken apart under its creation
/// record; any other tree is taken apart on Git's own worktree listing, with
/// its branch left where it is.
pub fn remove_target_previewed(
    repo_root: &Path,
    target: &WorktreeTarget,
    token: &str,
    delete_unmerged_branch: bool,
) -> Result<(), WorktreeError> {
    match &target.role {
        WorktreeRole::Linked { slug: Some(slug) } => {
            remove_previewed_worktree(repo_root, slug, token, delete_unmerged_branch)
        }
        WorktreeRole::MainCheckout => Err(WorktreeError::MainCheckout {
            path: target.path().to_path_buf(),
        }),
        WorktreeRole::Linked { slug: None } => {
            remove_foreign(repo_root, target, Authorization::Previewed(token))
        }
    }
}

/// Take a worktree apart for the session that worked in it, with the loss
/// already accepted. The branch of a tree this product created goes with it;
/// the branch of a tree entered from elsewhere stays.
pub fn remove_target(repo_root: &Path, target: &WorktreeTarget) -> Result<(), WorktreeError> {
    match &target.role {
        WorktreeRole::Linked { slug: Some(slug) } => remove_worktree(repo_root, slug),
        WorktreeRole::MainCheckout => Err(WorktreeError::MainCheckout {
            path: target.path().to_path_buf(),
        }),
        WorktreeRole::Linked { slug: None } => {
            remove_foreign(repo_root, target, Authorization::Discard)
        }
    }
}

/// Remove a linked worktree this product did not create, with its lease held
/// for the whole act.
///
/// Such a tree has no creation record and no leftover record, because both are
/// written per slug and this tree has none. What stands in their place is
/// Git's own registration: the removal asks Git to take the tree away, and
/// only a directory Git left behind after that is walked. The branch is never
/// deleted: this product did not make it, so it is not the removal's to
/// destroy.
fn remove_foreign(
    repo_root: &Path,
    target: &WorktreeTarget,
    authorization: Authorization<'_>,
) -> Result<(), WorktreeError> {
    let repo_root = canonicalize(repo_root)?;
    if target.identity.locked {
        return Err(WorktreeError::Locked {
            path: target.path().to_path_buf(),
            reason: "Git records a lock on it".to_string(),
        });
    }
    // The lease is taken before the state is read, so nothing can begin
    // working in the tree between the comparison and the act.
    let lease = WorktreeLease::acquire_for(&repo_root, target)?;
    let removed = remove_foreign_verified(&repo_root, target, authorization);
    // The lease file is this removal's own residue either way: on success the
    // tree it guarded is gone, and on failure a later attempt takes a fresh
    // lease before it acts again.
    lease.discard();
    removed
}

/// The verified part of removing a tree this product did not create.
fn remove_foreign_verified(
    repo_root: &Path,
    target: &WorktreeTarget,
    authorization: Authorization<'_>,
) -> Result<(), WorktreeError> {
    let path = target.path();
    // The numbers naming the directory are read before the checks and compared
    // again right before the walk deletes, so a directory replaced in between
    // is reported rather than deleted.
    let stamp = TargetStamp::read(path)?;
    // The confirmed preview is compared here, at the last point before Git is
    // asked, so a tree that moved since the preview is reported as changed
    // instead of being removed.
    if let Authorization::Previewed(token) = authorization
        && preview_foreign(repo_root, target)?.token != token
    {
        return Err(WorktreeError::Stale {
            slug: target.name(),
        });
    }
    if let Some(stamp) = stamp
        && let Err(reason) = stamp.verify(path)
    {
        return Err(WorktreeError::Identity {
            path: path.to_path_buf(),
            reason,
        });
    }
    let path_text = path.to_string_lossy().into_owned();
    run_git(repo_root, &["worktree", "remove", "--force", &path_text])?;
    // Git takes the directory with it. A directory still there afterwards is
    // one this removal has to take away itself, under the same numbers the
    // checks above agreed to. There is no record to finish it later, so a walk
    // that fails names the directory and leaves it whole.
    match path.try_exists() {
        Ok(false) => Ok(()),
        Ok(true) => remove_tree(path, stamp).map_err(|reason| WorktreeError::CleanupPending {
            path: path.to_path_buf(),
            reason,
        }),
        Err(error) => Err(WorktreeError::CleanupPending {
            path: path.to_path_buf(),
            reason: format!(
                "git took its registration away, and whether the directory is gone could not be checked: {error}"
            ),
        }),
    }
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
    /// The user accepted the loss of the tree and chose to keep its branch:
    /// the tree is removed with the same force, and an unmerged branch stays
    /// where it is.
    AuthorizedKeepBranch,
}

impl RemovalForce {
    fn remove_args(self) -> Vec<&'static str> {
        match self {
            Self::Conservative => vec!["worktree", "remove"],
            Self::AuthorizedDiscard | Self::AuthorizedKeepBranch => {
                vec!["worktree", "remove", "--force"]
            }
        }
    }

    /// What this removal is allowed to do, as the leftover record keeps it.
    fn intent(self) -> RemovalIntent {
        match self {
            Self::Conservative => RemovalIntent::Conservative,
            Self::AuthorizedDiscard => RemovalIntent::Discard,
            Self::AuthorizedKeepBranch => RemovalIntent::KeepBranch,
        }
    }

    /// The force a leftover record replays.
    fn from_intent(intent: RemovalIntent) -> Self {
        match intent {
            RemovalIntent::Conservative => Self::Conservative,
            RemovalIntent::Discard => Self::AuthorizedDiscard,
            RemovalIntent::KeepBranch => Self::AuthorizedKeepBranch,
        }
    }
}

/// Remove the directory Git registered for a slug, then delete its branch,
/// with the lease held for the whole act.
///
/// The lease is taken rather than asked about, so a session that acquires it
/// after a probe cannot begin working in a tree this call is deleting. A
/// previewed removal compares its token inside that lease, so the state the
/// user confirmed is still the state the act begins from. Inside it the act is
/// verified before anything is removed: the slug has to carry a record this
/// version reads, the directory has to resolve to the tree that record names,
/// and Git must not have it locked. A directory at the managed path without
/// that evidence is left alone whatever Git has registered there.
///
/// Each step runs only after the one before it is confirmed, and the record is
/// cleared last. A failure before the directory is gone leaves the directory,
/// whatever it still holds, its branch and its record. A branch Git refuses to
/// delete once the directory is gone answers RemovalIncomplete, naming what
/// survived instead of a Git message about a step that never ran, and the
/// record of the target lets the next attempt finish only that branch.
fn remove_linked(
    repo_root: &Path,
    slug: &str,
    force: RemovalForce,
    authorization: Authorization<'_>,
) -> Result<(), WorktreeError> {
    let lease = WorktreeLease::acquire(repo_root, slug)?;
    let checked = remove_verified(repo_root, slug, force, authorization);
    match checked {
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

/// The verified part of a removal: the checks that establish the object, the
/// preview the user confirmed, then the Git steps that take it apart.
fn remove_verified(
    repo_root: &Path,
    slug: &str,
    force: RemovalForce,
    authorization: Authorization<'_>,
) -> Result<(), WorktreeError> {
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
            return finish_pending(&repo, slug, &record, &pending);
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
    // The confirmed preview is re-read here rather than when this act began,
    // because this is the last point before Git is asked to change the tree. A
    // tree that moved since the preview answers Stale instead of being
    // removed. The re-read produces the token through the same path the
    // preview did, so the two describe the same facts.
    if let Authorization::Previewed(token) = authorization
        && preview_remove_worktree(repo_root, slug)?.token != token
    {
        return Err(WorktreeError::Stale {
            slug: slug.to_string(),
        });
    }
    // The record of the target goes down before Git is asked to take the
    // registration apart, so a removal that stops, or a process that dies,
    // between the two halves leaves a target the next attempt can finish.
    // While Git still lists the tree the record is inert: the next attempt
    // clears it and takes the ordinary path. A record that cannot be stored
    // stops the removal here, while the tree is still whole: past this point
    // the registration can be detached, and a directory left without a record
    // is one no later attempt knows how to finish.
    write_pending(
        repo_root,
        &PendingCleanup::new(slug, &path, stamp, force.intent()),
    )?;
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
///
/// The branch step replays the intent the record was written with, not the
/// force of the caller finishing it. No token can be compared here: the tree
/// it described is gone, so the authorization that took the removal this far
/// is the one that finishes it.
fn finish_pending(
    repo_root: &Path,
    slug: &str,
    record: &ManagedWorktree,
    pending: &PendingCleanup,
) -> Result<(), WorktreeError> {
    let force = RemovalForce::from_intent(pending.intent());
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
/// A merged branch still falls back to the flag that overrides the refusal,
/// because the work is in the base branch either way and the branch name is
/// all that is left to delete. The conservative caller never overrides Git;
/// the user's choice to keep unmerged work may, because what it keeps is
/// unmerged work and this branch has none.
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
    // An unmerged branch the user chose to keep is not a step that failed: the
    // tree is gone and the branch stays where it was, which is what the
    // removal was asked for.
    if !merged && matches!(force, RemovalForce::AuthorizedKeepBranch) {
        return Ok(());
    }
    let discard_allowed = matches!(force, RemovalForce::AuthorizedDiscard);
    // A merged branch is work the base branch already holds, so the fallback
    // that overrides Git's refusal stays available for a user who chose to
    // keep unmerged work; only the conservative caller never overrides it.
    let may_override = match force {
        RemovalForce::AuthorizedDiscard => true,
        RemovalForce::AuthorizedKeepBranch => merged,
        RemovalForce::Conservative => false,
    };
    let first = if merged || !discard_allowed {
        "-d"
    } else {
        "-D"
    };
    match run_git(repo_root, &["branch", first, &branch]) {
        Ok(_) => Ok(()),
        Err(error) => {
            if may_override && merged && run_git(repo_root, &["branch", "-D", &branch]).is_ok() {
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
