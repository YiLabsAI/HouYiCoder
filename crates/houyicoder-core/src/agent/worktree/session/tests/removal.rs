//! What a removal does with a leftover Git already detached, and what it
//! leaves of everything else.

use std::fs;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
#[cfg(unix)]
use {
    std::fs::Permissions,
    std::os::unix::fs::{PermissionsExt, symlink},
};

use serde_json::json;

use super::super::*;
use super::make_repo;
use crate::agent::worktree::cleanup::{
    PendingCleanup, RemovalIntent, lookup_pending, write_pending,
};
use crate::agent::worktree::identify::TargetStamp;
use crate::agent::worktree::preview::preview_remove_worktree;
#[cfg(unix)]
use crate::agent::worktree::record::state_dir;
use crate::agent::worktree::record::{
    ManagedWorktree, pending_path, read_managed_worktree, write_managed_worktree,
};

/// Build the leftover a stopped removal leaves: a creation record, a branch, a
/// directory Git does not register, and the cleanup record naming it.
fn leftover(tag: &str, slug: &str) -> (PathBuf, PathBuf) {
    let repo = make_repo(tag);
    let path = worktree_path_for(&repo, slug);
    fs::create_dir_all(path.join("nested")).expect("leftover dir");
    write(path.join("nested").join("work.txt"), "work").expect("leftover file");
    write_managed_worktree(&repo, &ManagedWorktree::new(slug, None)).expect("creation record");
    run_git(&repo, &["branch", &worktree_branch_name(slug)]).expect("branch");
    let resolved = dunce::canonicalize(&path).expect("resolve the leftover");
    let stamp = TargetStamp::read(&resolved).expect("read the stamp");
    write_pending(
        &repo,
        &PendingCleanup::new(slug, &resolved, stamp, RemovalIntent::Conservative),
    )
    .expect("cleanup record");
    (repo, resolved)
}

fn branch_exists(repo: &Path, slug: &str) -> bool {
    let branch = worktree_branch_name(slug);
    run_git(
        repo,
        &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
    )
    .is_ok()
}

/// A leftover is finished by the removal that finds its record: the directory,
/// the branch, and both records go, and nothing else is touched.
#[test]
fn test_leftover_resumes() {
    let (repo, path) = leftover("leftover-resume", "left-behind");

    remove_clean_worktree(&repo, "left-behind").expect("the leftover is finished");

    assert!(!path.exists(), "the leftover is gone");
    assert!(!branch_exists(&repo, "left-behind"), "the branch is gone");
    assert!(
        read_managed_worktree(&repo, "left-behind").is_none(),
        "the creation record is gone"
    );
    assert!(
        lookup_pending(&repo, "left-behind")
            .expect("read the record")
            .is_none(),
        "the cleanup record is gone"
    );
    drop(fs::remove_dir_all(&repo));
}

/// Put a commit on a leftover's branch that its base does not hold, so a
/// deletion that respects Git's caution and one that overrides it end
/// differently.
fn commit_ahead(repo: &Path, slug: &str) {
    let branch = worktree_branch_name(slug);
    let head = run_git(repo, &["rev-parse", "HEAD"]).expect("read the base commit");
    let tree = run_git(repo, &["rev-parse", "HEAD^{tree}"]).expect("read the base tree");
    let ahead = run_git(repo, &["commit-tree", &tree, "-p", &head, "-m", "wip"])
        .expect("make a commit on the branch");
    run_git(
        repo,
        &["update-ref", &format!("refs/heads/{branch}"), ahead.trim()],
    )
    .expect("move the branch onto it");
}

/// The leftover finishes with the choice the removal that wrote it was given:
/// one that stopped after the user kept the branch still keeps it, whatever a
/// later attempt arrives able to discard.
#[test]
fn test_leftover_replays_keep_branch() {
    let (repo, path) = leftover("leftover-keep-branch", "kept-work");
    commit_ahead(&repo, "kept-work");
    write_pending(
        &repo,
        &PendingCleanup::new("kept-work", &path, None, RemovalIntent::KeepBranch),
    )
    .expect("record the keep choice");

    remove_worktree(&repo, "kept-work").expect("the leftover is finished");

    assert!(!path.exists(), "the directory is gone");
    assert!(
        branch_exists(&repo, "kept-work"),
        "the choice the record carries stands over the caller's discard"
    );
    drop(fs::remove_dir_all(&repo));
}

/// The other half: a leftover written by a removal the user accepted as a
/// discard deletes the unmerged branch even when the attempt that finishes it
/// would only remove what Git removes on its own.
#[test]
fn test_leftover_replays_discard() {
    let (repo, path) = leftover("leftover-discard", "dropped-work");
    commit_ahead(&repo, "dropped-work");
    write_pending(
        &repo,
        &PendingCleanup::new("dropped-work", &path, None, RemovalIntent::Discard),
    )
    .expect("record the discard choice");

    remove_clean_worktree(&repo, "dropped-work").expect("the leftover is finished");

    assert!(!path.exists(), "the directory is gone");
    assert!(
        !branch_exists(&repo, "dropped-work"),
        "the choice the record carries stands over the caller's caution"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A record naming a directory other than the one the slug resolves to is
/// refused: the leftover is an object that was recorded, not whatever now sits
/// where the slug points.
#[test]
fn test_leftover_refuses_other_path() {
    let (repo, path) = leftover("leftover-other", "moved");
    let elsewhere = repo.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("another directory");
    let elsewhere = dunce::canonicalize(&elsewhere).expect("resolve it");
    write_pending(
        &repo,
        &PendingCleanup::new("moved", &elsewhere, None, RemovalIntent::Conservative),
    )
    .expect("repoint the record");

    let error = remove_clean_worktree(&repo, "moved").expect_err("the record points elsewhere");
    assert!(
        matches!(error, WorktreeError::CleanupPending { .. }),
        "expected CleanupPending, got {error:?}"
    );
    assert!(path.exists(), "the directory the slug names is left alone");
    assert!(
        elsewhere.exists(),
        "the directory the record names is left alone"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A leftover whose directory is already gone leaves only the branch and the
/// records, which is what the previous attempt stopped before.
#[test]
fn test_leftover_when_dir_gone() {
    let (repo, path) = leftover("leftover-gone", "removed-dir");
    fs::remove_dir_all(&path).expect("drop the directory by hand");

    remove_clean_worktree(&repo, "removed-dir").expect("the rest is finished");

    assert!(!branch_exists(&repo, "removed-dir"), "the branch is gone");
    assert!(
        read_managed_worktree(&repo, "removed-dir").is_none(),
        "the records are gone"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A leftover whose directory was replaced after the record was written is
/// refused, and the replacement is left alone: the record names a directory,
/// not a path.
#[cfg(unix)]
#[test]
fn test_leftover_replaced_kept() {
    let (repo, path) = leftover("leftover-replaced", "swapped");
    fs::rename(&path, repo.join("moved")).expect("move the recorded directory away");
    fs::create_dir_all(&path).expect("a new directory in its place");
    write(path.join("keep.txt"), "keep").expect("a file in the replacement");

    let error = remove_clean_worktree(&repo, "swapped").expect_err("the directory was replaced");

    assert!(
        matches!(error, WorktreeError::CleanupPending { .. }),
        "expected CleanupPending, got {error:?}"
    );
    assert!(
        path.join("keep.txt").exists(),
        "the replacement is untouched"
    );
    assert!(branch_exists(&repo, "swapped"), "the branch is kept");
    drop(fs::remove_dir_all(&repo));
}

/// A link inside the leftover is dropped while what it names survives: the
/// walk removes the link by its own name rather than following it.
#[cfg(unix)]
#[test]
fn test_leftover_keeps_outside() {
    let (repo, path) = leftover("leftover-link", "linked");
    let outside = repo.join("outside");
    fs::create_dir_all(&outside).expect("outside dir");
    write(outside.join("keep.txt"), "keep").expect("outside file");
    symlink(&outside, path.join("link")).expect("link into the leftover");

    remove_clean_worktree(&repo, "linked").expect("the leftover is finished");

    assert!(!path.exists(), "the leftover is gone");
    assert!(
        outside.join("keep.txt").exists(),
        "what the link named is untouched"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A branch proven merged is deleted even when the repository is checked out
/// somewhere the work is not: Git refuses the cautious flag there, and the
/// authorized discard carries the branch name the rest of the way.
#[test]
fn test_merged_branch_deleted_elsewhere() {
    let repo = make_repo("merged-elsewhere");
    let slug = "merged-work";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    write(created.worktree_path.join("work.txt"), "work").expect("write work");
    run_git(&created.worktree_path, &["add", "work.txt"]).expect("stage the work");
    run_git(&created.worktree_path, &["commit", "-m", "work", "-q"]).expect("commit the work");
    run_git(&repo, &["merge", "--ff-only", &worktree_branch_name(slug)]).expect("merge the work");
    run_git(&repo, &["checkout", "-b", "elsewhere", "HEAD~1"])
        .expect("leave the merged work behind");

    remove_worktree(&repo, slug).expect("the merged branch is deleted");

    assert!(!branch_exists(&repo, slug), "the merged branch is gone");
    drop(fs::remove_dir_all(&repo));
}

/// A merged branch is deleted even when the user chose to keep unmerged work:
/// that choice keeps unmerged work, and this branch has none.
#[test]
fn test_keep_option_deletes_merged() {
    let repo = make_repo("merged-kept");
    let slug = "kept-merge";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    write(created.worktree_path.join("work.txt"), "work").expect("write work");
    run_git(&created.worktree_path, &["add", "work.txt"]).expect("stage the work");
    run_git(&created.worktree_path, &["commit", "-m", "work", "-q"]).expect("commit the work");
    run_git(&repo, &["merge", "--ff-only", &worktree_branch_name(slug)]).expect("merge the work");
    run_git(&repo, &["checkout", "-b", "elsewhere", "HEAD~1"])
        .expect("leave the merged work behind");
    let token = preview_remove_worktree(&repo, slug)
        .expect("preview the tree")
        .token;

    remove_previewed_worktree(&repo, slug, &token, false).expect("the merged branch is deleted");

    assert!(!branch_exists(&repo, slug), "the merged branch is gone");
    drop(fs::remove_dir_all(&repo));
}

/// Removing one worktree leaves every other registered worktree alone: the act
/// is scoped to its own slug, and no sweep of the repository follows it.
#[test]
fn test_remove_leaves_sibling() {
    let repo = make_repo("remove-sibling");
    let kept = get_or_create_worktree(&repo, "kept-tree").expect("create the sibling");
    get_or_create_worktree(&repo, "gone-tree").expect("create the tree to remove");

    remove_clean_worktree(&repo, "gone-tree").expect("remove one tree");

    assert!(kept.worktree_path.exists(), "the sibling still exists");
    let listed = run_git(&repo, &["worktree", "list", "--porcelain"]).expect("list worktrees");
    let sibling = dunce::canonicalize(&kept.worktree_path).expect("resolve the sibling");
    assert!(
        listed.contains(&sibling.to_string_lossy().into_owned()),
        "the sibling is still registered"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A record naming a different worktree is refused: the slug being removed and
/// the record have to describe the same leftover.
#[test]
fn test_leftover_record_other_slug() {
    let repo = make_repo("leftover-other-slug");
    write_managed_worktree(&repo, &ManagedWorktree::new("mine", None)).expect("creation record");
    let target = repo.join("leftover");
    fs::create_dir_all(&target).expect("a directory to name");
    let record = pending_path(&repo, "mine");
    fs::create_dir_all(record.parent().expect("state dir")).expect("state dir");
    write(
        &record,
        json!({"schema": 1, "slug": "theirs", "path": target}).to_string(),
    )
    .expect("a record naming another worktree");

    let error =
        remove_clean_worktree(&repo, "mine").expect_err("the record names another worktree");

    assert!(
        matches!(error, WorktreeError::CleanupPending { .. }),
        "expected CleanupPending, got {error:?}"
    );
    assert!(
        target.exists(),
        "the directory the record names is left alone"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A managed root that is a file leaves the leftover unresolvable, which is
/// refused rather than read as a directory that is already gone.
#[test]
fn test_leftover_blocked_path() {
    let repo = make_repo("leftover-blocked");
    write_managed_worktree(&repo, &ManagedWorktree::new("blocked", None)).expect("creation record");
    let root = worktrees_dir(&repo);
    fs::create_dir_all(root.parent().expect("state parent")).expect("state parent");
    write(&root, "not a directory").expect("a file where the managed root goes");
    let target = repo.join("leftover");
    fs::create_dir_all(&target).expect("a directory to name");
    let resolved = dunce::canonicalize(&target).expect("resolve the target");
    write_pending(
        &repo,
        &PendingCleanup::new("blocked", &resolved, None, RemovalIntent::Conservative),
    )
    .expect("cleanup record");

    let error =
        remove_clean_worktree(&repo, "blocked").expect_err("the managed root is not a directory");

    assert!(
        matches!(error, WorktreeError::CleanupPending { .. }),
        "expected CleanupPending, got {error:?}"
    );
    assert!(
        target.exists(),
        "the directory the record names is left alone"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A leftover that now resolves outside the managed root is refused even when
/// the record names the directory it resolves to: a link put there after the
/// removal stopped is not the leftover that removal left.
#[cfg(unix)]
#[test]
fn test_leftover_link_outside() {
    let repo = make_repo("leftover-outside");
    let slug = "linked-out";
    write_managed_worktree(&repo, &ManagedWorktree::new(slug, None)).expect("creation record");
    let outside = repo.join("outside");
    fs::create_dir_all(&outside).expect("outside dir");
    write(outside.join("keep.txt"), "keep").expect("outside file");
    let root = worktrees_dir(&repo);
    fs::create_dir_all(&root).expect("managed root");
    symlink(&outside, root.join(flatten_slug(slug))).expect("link where the worktree goes");
    let resolved = dunce::canonicalize(&outside).expect("resolve the outside directory");
    write_pending(
        &repo,
        &PendingCleanup::new(slug, &resolved, None, RemovalIntent::Conservative),
    )
    .expect("cleanup record");

    let error = remove_clean_worktree(&repo, slug).expect_err("the leftover resolves outside");

    assert!(
        matches!(error, WorktreeError::CleanupPending { .. }),
        "expected CleanupPending, got {error:?}"
    );
    assert!(
        outside.join("keep.txt").exists(),
        "the directory outside is untouched"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A leftover that cannot be taken apart keeps both the directory and the
/// record, so the next attempt has the same object to finish.
#[cfg(unix)]
#[test]
fn test_leftover_keeps_unremovable() {
    let (repo, path) = leftover("leftover-stuck", "stuck");
    let nested = path.join("nested");
    let mode = fs::metadata(&nested)
        .expect("nested dir")
        .permissions()
        .mode();
    fs::set_permissions(&nested, Permissions::from_mode(0o500))
        .expect("make the nested directory unwritable");

    let error = remove_clean_worktree(&repo, "stuck").expect_err("the leftover cannot be removed");

    fs::set_permissions(&nested, Permissions::from_mode(mode)).expect("restore the mode");
    assert!(
        matches!(error, WorktreeError::CleanupPending { .. }),
        "expected CleanupPending, got {error:?}"
    );
    assert!(path.exists(), "the leftover is kept");
    assert!(
        lookup_pending(&repo, "stuck")
            .expect("read the record")
            .is_some(),
        "the record is kept"
    );

    // Once whatever held the directory lets go, the same act finishes it.
    remove_clean_worktree(&repo, "stuck").expect("the leftover is finished on the next attempt");
    assert!(!path.exists(), "the leftover is gone");
    assert!(
        lookup_pending(&repo, "stuck")
            .expect("read the record")
            .is_none(),
        "the record goes with it"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A removal that stopped at the branch, with the directory already gone, is
/// finished by the next attempt: the record of the target outlives the
/// directory it named, so the branch is the only step left.
#[test]
fn test_branch_failure_resumes() {
    let repo = make_repo("branch-resume");
    let slug = "held-branch";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let branch = worktree_branch_name(slug);
    let lock = repo
        .join(".git")
        .join("refs")
        .join("heads")
        .join(format!("{branch}.lock"));
    fs::create_dir_all(lock.parent().expect("refs dir")).expect("refs dir");
    write(&lock, "").expect("hold the branch ref lock");

    let error = remove_clean_worktree(&repo, slug).expect_err("the branch cannot be deleted");

    assert!(
        matches!(error, WorktreeError::RemovalIncomplete { .. }),
        "expected RemovalIncomplete, got {error:?}"
    );
    assert!(
        !created.worktree_path.exists(),
        "the directory went before the branch"
    );
    fs::remove_file(&lock).expect("release the branch ref lock");

    remove_clean_worktree(&repo, slug).expect("the branch step is finished on the next attempt");

    assert!(!branch_exists(&repo, slug), "the branch is gone");
    drop(fs::remove_dir_all(&repo));
}

/// A removal whose branch an earlier attempt already deleted finishes instead
/// of failing: the branch is not a step left to do.
#[test]
fn test_removal_finishes_without_branch() {
    let (repo, path) = leftover("branch-gone", "no-branch");
    run_git(&repo, &["branch", "-D", &worktree_branch_name("no-branch")])
        .expect("delete the branch by hand");

    remove_clean_worktree(&repo, "no-branch").expect("the rest is finished");

    assert!(!path.exists(), "the leftover is gone");
    assert!(!branch_exists(&repo, "no-branch"), "no branch is left");
    drop(fs::remove_dir_all(&repo));
}

/// A removal the user authorized deletes a branch holding unmerged work: the
/// loss is what the authorization accepted, and it is the only path allowed to
/// use the flag that overrides Git's refusal.
#[test]
fn test_discard_deletes_unmerged_branch() {
    let repo = make_repo("discard-unmerged");
    let slug = "unmerged-work";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    write(created.worktree_path.join("work.txt"), "work").expect("write work");
    run_git(&created.worktree_path, &["add", "work.txt"]).expect("stage the work");
    run_git(&created.worktree_path, &["commit", "-m", "work", "-q"]).expect("commit the work");

    remove_worktree(&repo, slug).expect("the authorized discard removes it");

    assert!(!created.worktree_path.exists(), "the tree is gone");
    assert!(!branch_exists(&repo, slug), "the unmerged branch is gone");
    drop(fs::remove_dir_all(&repo));
}

/// A cleanup record that cannot be stored stops the removal while the tree is
/// whole: past that point Git can detach the registration, and a directory left
/// without a record is one no later attempt knows how to finish.
#[cfg(unix)]
#[test]
fn test_record_failure_keeps_tree() {
    let repo = make_repo("record-fails");
    let slug = "sealed";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let pending = state_dir(&repo).join("pending");
    fs::create_dir_all(&pending).expect("pending dir");
    seal(&pending, 0o500);

    let error = remove_worktree(&repo, slug).expect_err("the record cannot be stored");

    assert!(
        matches!(error, WorktreeError::State { .. }),
        "got {error:?}"
    );
    assert!(created.worktree_path.exists(), "the tree is kept");
    assert!(branch_exists(&repo, slug), "the branch is kept");
    assert!(
        run_git(&repo, &["worktree", "list", "--porcelain"])
            .expect("list")
            .contains(slug),
        "Git still registers the tree"
    );
    seal(&pending, 0o700);
    drop(fs::remove_dir_all(&repo));
}

/// Change a directory's permission bits.
#[cfg(unix)]
fn seal(dir: &Path, mode: u32) {
    let mut perms = fs::metadata(dir).expect("metadata").permissions();
    perms.set_mode(mode);
    fs::set_permissions(dir, perms).expect("set permissions");
}

/// A branch Git cannot be asked about is reported, not read as absent: only
/// the probe's own answer for a revision it cannot resolve means the branch is
/// gone, and a removal that assumes it is would clear the record of a branch
/// still holding work. The repository is taken away here, so the probe ends in
/// a status that is neither present nor absent.
#[test]
fn test_removal_reports_unconfirmed_branch() {
    let (repo, path) = leftover("probe-fails", "orphan");
    fs::remove_dir_all(&path).expect("take the recorded directory away");
    fs::remove_dir_all(repo.join(".git")).expect("take the repository away");

    let error = remove_worktree(&repo, "orphan").expect_err("Git cannot be asked");

    assert!(
        matches!(error, WorktreeError::RemovalIncomplete { .. }),
        "got {error:?}"
    );
    drop(fs::remove_dir_all(&repo));
}

/// One removal leaves nothing behind: no directory, no branch, no Git
/// registration, and no record, even when the tree holds a build directory and
/// the agent's own settings files.
#[test]
fn test_remove_leaves_no_residue() {
    let repo = make_repo("zero-residue");
    let slug = "full-tree";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let path = created.worktree_path.clone();
    let build = path.join("target").join("debug");
    fs::create_dir_all(&build).expect("build dir");
    write(build.join("out.bin"), "out").expect("build output");
    fs::create_dir_all(path.join(".claude")).expect("settings dir");
    write(path.join(".claude").join("settings.json"), "{}").expect("settings file");
    write(path.join("SKILL.md"), "# skill").expect("skill file");

    remove_worktree(&repo, slug).expect("the tree is removed");

    assert!(!path.exists(), "the directory is gone");
    assert!(!branch_exists(&repo, slug), "the branch is gone");
    let listed = run_git(&repo, &["worktree", "list", "--porcelain"]).expect("list worktrees");
    assert!(
        !listed.contains(flatten_slug(slug).as_str()),
        "Git no longer registers it"
    );
    assert!(
        read_managed_worktree(&repo, slug).is_none(),
        "the creation record is gone"
    );
    assert!(
        lookup_pending(&repo, slug)
            .expect("read the record")
            .is_none(),
        "the cleanup record is gone"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A worktree named after the state file an earlier removal layout used is a
/// worktree like any other: removing one name leaves the other's records where
/// they are.
#[test]
fn test_remove_keeps_namesake_records() {
    let repo = make_repo("namesake");
    let plain = get_or_create_worktree(&repo, "api").expect("create the plain name");
    let namesake = get_or_create_worktree(&repo, "api.cleanup").expect("create the namesake");

    remove_clean_worktree(&repo, "api").expect("remove the plain name");

    assert!(!plain.worktree_path.exists(), "the removed tree is gone");
    assert!(namesake.worktree_path.exists(), "the namesake still exists");
    assert!(
        read_managed_worktree(&repo, "api.cleanup").is_some(),
        "the namesake keeps its creation record"
    );
    remove_clean_worktree(&repo, "api.cleanup").expect("the namesake is still removable");
    drop(fs::remove_dir_all(&repo));
}

/// A removal that carries the token a preview returned acts on the tree that
/// preview showed: the directory, the branch and the records all go.
#[test]
fn test_removal_accepts_matching_preview() {
    let repo = make_repo("preview-match");
    let slug = "confirmed";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let path = dunce::canonicalize(&created.worktree_path).expect("resolve the tree");
    let token = preview_remove_worktree(&repo, slug).expect("preview").token;

    remove_previewed_worktree(&repo, slug, &token, true).expect("the confirmed removal runs");

    assert!(!path.exists(), "the directory is gone");
    assert!(!branch_exists(&repo, slug), "the branch is gone");
    assert!(
        read_managed_worktree(&repo, slug).is_none(),
        "the creation record goes with the tree"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A confirmed removal keeps an unmerged branch unless the confirmation said
/// otherwise: the tree goes, and the commits the preview counted stay
/// reachable on the branch.
#[test]
fn test_removal_keeps_unmerged_branch() {
    let repo = make_repo("preview-keep");
    let slug = "keep-me";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let path = dunce::canonicalize(&created.worktree_path).expect("resolve the tree");
    run_git(
        &created.worktree_path,
        &["commit", "--allow-empty", "-m", "wip"],
    )
    .expect("work on the branch");
    let preview = preview_remove_worktree(&repo, slug).expect("preview");
    assert_eq!(
        preview.unmerged_commits,
        Some(1),
        "the preview counts the work the branch holds"
    );

    remove_previewed_worktree(&repo, slug, &preview.token, false).expect("the tree is removed");

    assert!(!path.exists(), "the directory is gone");
    assert!(branch_exists(&repo, slug), "the branch outlives the tree");
    let tip = run_git(
        &repo,
        &["log", "-1", "--format=%s", &worktree_branch_name(slug)],
    )
    .expect("read the branch tip");
    assert_eq!(tip.trim(), "wip", "the work stays reachable on the branch");
    drop(fs::remove_dir_all(&repo));
}

/// The other half of the choice: the same unmerged branch goes when the
/// confirmation asked for it.
#[test]
fn test_removal_discards_unmerged_branch() {
    let repo = make_repo("preview-discard");
    let slug = "discard-me";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let path = dunce::canonicalize(&created.worktree_path).expect("resolve the tree");
    run_git(
        &created.worktree_path,
        &["commit", "--allow-empty", "-m", "wip"],
    )
    .expect("work on the branch");
    let preview = preview_remove_worktree(&repo, slug).expect("preview");

    remove_previewed_worktree(&repo, slug, &preview.token, true).expect("the tree is removed");

    assert!(!path.exists(), "the directory is gone");
    assert!(!branch_exists(&repo, slug), "the branch goes with the tree");
    drop(fs::remove_dir_all(&repo));
}

/// A tree that changed after its preview is not the tree the user confirmed, so
/// the removal stops before anything is taken apart and answers Stale. Every
/// part of the object survives, which is what a second preview acts on.
#[test]
fn test_removal_refuses_changed_preview() {
    let repo = make_repo("preview-stale");
    let slug = "moved-on";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let path = dunce::canonicalize(&created.worktree_path).expect("resolve the tree");
    let token = preview_remove_worktree(&repo, slug).expect("preview").token;
    write(path.join("added-after-preview.txt"), "work").expect("change the tree");

    let error =
        remove_previewed_worktree(&repo, slug, &token, true).expect_err("the tree moved on");

    assert!(
        matches!(error, WorktreeError::Stale { ref slug } if slug == "moved-on"),
        "expected Stale, got {error:?}"
    );
    assert!(path.exists(), "the directory survives");
    assert!(branch_exists(&repo, slug), "the branch survives");
    assert!(
        read_managed_worktree(&repo, slug).is_some(),
        "the creation record survives"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A stale record for a worktree Git still registers describes nothing, so the
/// ordinary removal runs and the record goes with it.
#[test]
fn test_pending_cleared_when_registered() {
    let repo = make_repo("pending-registered");
    let slug = "still-there";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let path = dunce::canonicalize(&created.worktree_path).expect("resolve the tree");
    write_pending(
        &repo,
        &PendingCleanup::new(slug, &path, None, RemovalIntent::Conservative),
    )
    .expect("stale cleanup record");

    remove_clean_worktree(&repo, slug).expect("the tree is removed");

    assert!(!path.exists(), "the tree is gone");
    assert!(!branch_exists(&repo, slug), "the branch is gone");
    assert!(
        lookup_pending(&repo, slug)
            .expect("read the record")
            .is_none(),
        "the stale record is gone"
    );
    drop(fs::remove_dir_all(&repo));
}
