//! What a removal preview reports for a managed worktree, and which states it
//! refuses to preview at all.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::id;

#[cfg(unix)]
use std::{fs::rename, os::unix::fs::symlink};

use super::super::changes::WorktreeChanges;
use super::super::identify::WorktreeIdentity;
use super::super::record::ManagedWorktree;
use super::super::record::WorktreeLease;
use super::super::record::read_managed_worktree;
use super::super::record::record_path;
use super::super::record::write_managed_worktree;
use super::super::session::WorktreeError;
use super::super::session::get_or_create_worktree;
use super::super::session::worktree_branch_name;
use super::super::session::worktree_path_for;
use super::*;

/// A throwaway repository with one commit, so a preview has a base branch and
/// a commit to ask its merge question against.
fn make_repo(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-wt-preview-{tag}-{}", id()));
    drop(remove_dir_all(&dir));
    create_dir_all(&dir).expect("repo dir");
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@x"]);
    git(&dir, &["config", "user.name", "t"]);
    git(&dir, &["commit", "--allow-empty", "-m", "init", "-q"]);
    dir
}

#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding, not an engine spawn"
)]
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// Create a worktree through the enter path, which records it.
fn managed(repo: &Path, slug: &str) -> PathBuf {
    let created = get_or_create_worktree(repo, slug).expect("create worktree");
    assert!(
        read_managed_worktree(repo, slug).is_some(),
        "creation records the worktree"
    );
    created.worktree_path
}

/// Create a worktree directly, the way another tool would, leaving no record.
fn unmanaged(repo: &Path, slug: &str) -> PathBuf {
    let path = worktree_path_for(repo, slug);
    create_dir_all(path.parent().expect("managed dir")).expect("create managed dir");
    git(
        repo,
        &[
            "worktree",
            "add",
            "-b",
            &worktree_branch_name(slug),
            &path.to_string_lossy(),
            "HEAD",
        ],
    );
    path
}

/// A worktree nothing changed in reports zero of every class and a branch that
/// sits exactly on its base, which is merged by definition.
#[test]
fn test_preview_reports_clean_merged() {
    let repo = make_repo("clean");
    let path = managed(&repo, "clean");

    let preview = preview_remove_worktree(&repo, "clean").expect("preview");
    assert_eq!(preview.identity.path, path);
    assert_eq!(preview.changes.tracked_dirty, 0, "{:?}", preview.changes);
    assert_eq!(preview.changes.untracked, 0, "{:?}", preview.changes);
    assert!(
        preview.changes.ignored_build_dirs.is_empty(),
        "{:?}",
        preview.changes
    );
    assert_eq!(preview.changes.ignored_unknown, 0, "{:?}", preview.changes);
    assert!(!preview.locked, "nobody locked the tree");
    assert!(
        matches!(preview.merge, MergeState::Merged { .. }),
        "{:?}",
        preview.merge
    );
    assert!(!preview.leased, "nobody leased the tree");
    assert_eq!(
        preview.unmerged_commits,
        Some(0),
        "a merged branch holds no work"
    );
    assert_eq!(preview.token.len(), 16, "the token is a fixed-width digest");
    drop(remove_dir_all(&repo));
}

/// Each class is reported on its own: a known build directory is named, an
/// ignored file no build name covers is counted as unknown, and neither hides
/// the tracked or untracked counts.
#[test]
fn test_preview_classifies_changes() {
    let repo = make_repo("changes");
    let path = managed(&repo, "changes");
    write(path.join(".gitignore"), "target/\n.env.local\n").expect("write ignore rules");
    write(path.join("tracked.txt"), "one\n").expect("write tracked file");
    git(&path, &["add", ".gitignore", "tracked.txt"]);
    git(&path, &["commit", "-m", "seed", "-q"]);
    write(path.join("tracked.txt"), "two\n").expect("edit the tracked file");
    write(path.join("untracked.txt"), "new\n").expect("write untracked file");
    create_dir_all(path.join("target").join("debug")).expect("create build dir");
    write(path.join("target").join("debug").join("out"), "bin").expect("fill build dir");
    write(path.join(".env.local"), "secret\n").expect("write ignored file");

    let preview = preview_remove_worktree(&repo, "changes").expect("preview");
    assert_eq!(preview.changes.tracked_dirty, 1);
    assert_eq!(preview.changes.untracked, 1);
    assert_eq!(
        preview.changes.ignored_build_dirs,
        vec!["target".to_string()]
    );
    assert_eq!(preview.changes.ignored_unknown, 1);
    drop(remove_dir_all(&repo));
}

/// A commit the base branch does not contain reads as unmerged, and the same
/// branch reads as merged once the base has it.
#[test]
fn test_preview_reports_merge_state() {
    let repo = make_repo("merge");
    let path = managed(&repo, "feature");
    write(path.join("work.txt"), "done\n").expect("write work");
    git(&path, &["add", "work.txt"]);
    git(&path, &["commit", "-m", "feature work", "-q"]);

    let before = preview_remove_worktree(&repo, "feature").expect("preview");
    assert!(
        matches!(before.merge, MergeState::NotMerged { .. }),
        "{:?}",
        before.merge
    );
    assert_eq!(
        before.unmerged_commits,
        Some(1),
        "the unmerged commit is counted, not inferred from the state"
    );

    git(&repo, &["merge", "--ff-only", "worktree-feature"]);
    let after = preview_remove_worktree(&repo, "feature").expect("preview");
    assert!(
        matches!(after.merge, MergeState::Merged { .. }),
        "{:?}",
        after.merge
    );
    assert_eq!(after.unmerged_commits, Some(0));
    drop(remove_dir_all(&repo));
}

/// A record with no base branch leaves the merge question unasked, and the
/// count answers None rather than zero: a removal must not read "no base" as
/// "nothing would be lost".
#[test]
fn test_unknown_base_counts_none() {
    let repo = make_repo("nobase");
    managed(&repo, "orphan");
    write_managed_worktree(&repo, &ManagedWorktree::new("orphan", None))
        .expect("record without base");

    let preview = preview_remove_worktree(&repo, "orphan").expect("preview");
    assert_eq!(preview.merge, MergeState::Unknown);
    assert_eq!(preview.unmerged_commits, None);
    drop(remove_dir_all(&repo));
}

/// A base branch that no longer exists makes Git answer the merge question
/// with a failure rather than with ancestry, and that answer is Unknown: the
/// tree is not reported as unmerged work because the question was never
/// answered.
#[test]
fn test_missing_base_reports_unknown() {
    let repo = make_repo("gonebase");
    managed(&repo, "stale");
    write_managed_worktree(
        &repo,
        &ManagedWorktree::new("stale", Some("gone".to_string())),
    )
    .expect("record with a missing base");

    let preview = preview_remove_worktree(&repo, "stale").expect("preview");
    assert_eq!(preview.merge, MergeState::Unknown);
    assert_eq!(preview.unmerged_commits, None);
    drop(remove_dir_all(&repo));
}

/// A second preview of an unchanged tree repeats the token, and a file the
/// preview counts as changed moves it.
#[test]
fn test_token_repeats_unchanged() {
    let repo = make_repo("token");
    let path = managed(&repo, "steady");
    let first = preview_remove_worktree(&repo, "steady").expect("preview");
    let second = preview_remove_worktree(&repo, "steady").expect("preview again");
    assert_eq!(
        first.token, second.token,
        "an unchanged tree repeats its token"
    );

    write(path.join("new.txt"), "new\n").expect("write a file");
    let changed = preview_remove_worktree(&repo, "steady").expect("preview after change");
    assert_eq!(changed.changes.untracked, 1);
    assert_ne!(changed.token, first.token, "a change moves the token");
    drop(remove_dir_all(&repo));
}

/// The token names the history a removal would act on: the commit checked
/// out, the base the merge question was asked against, and how far the branch
/// has moved past it. Each one is shown moving alone, so the token is known to
/// follow each of them rather than only the last thing that changed.
#[test]
fn test_history_moves_token() {
    let repo = make_repo("history");
    let path = managed(&repo, "steady");
    write(repo.join("base.txt"), "base\n").expect("write a base commit file");
    git(&repo, &["add", "base.txt"]);
    git(&repo, &["commit", "-m", "base work", "-q"]);
    // A second branch stands on the base's own commit and is recorded as the
    // base, so a later move of that ref changes the count under one label.
    git(&repo, &["branch", "alias-base"]);
    write_managed_worktree(
        &repo,
        &ManagedWorktree::new("steady", Some("alias-base".to_string())),
    )
    .expect("record against the alias base");
    let settled = preview_remove_worktree(&repo, "steady").expect("preview");
    assert!(
        matches!(settled.merge, MergeState::Merged { .. }),
        "{:?}",
        settled.merge
    );
    assert_eq!(settled.unmerged_commits, Some(0));

    // The tree moves up to a commit the base already holds, so the head moves
    // and neither the merge answer nor the count does.
    git(&path, &["reset", "--hard", "alias-base"]);
    let moved = preview_remove_worktree(&repo, "steady").expect("preview after the reset");
    assert!(
        matches!(moved.merge, MergeState::Merged { .. }),
        "{:?}",
        moved.merge
    );
    assert_eq!(moved.unmerged_commits, Some(0));
    assert_ne!(moved.token, settled.token, "a moved head moves the token");

    // Two commits on top, then the base moves up to the first of them: the
    // branch is still unmerged against the same base, with one fewer commit
    // between them.
    git(&path, &["commit", "--allow-empty", "-m", "one", "-q"]);
    git(&path, &["commit", "--allow-empty", "-m", "two", "-q"]);
    let ahead_two = preview_remove_worktree(&repo, "steady").expect("preview two ahead");
    assert!(
        matches!(ahead_two.merge, MergeState::NotMerged { .. }),
        "{:?}",
        ahead_two.merge
    );
    assert_eq!(ahead_two.unmerged_commits, Some(2));
    git(&repo, &["branch", "-f", "alias-base", "worktree-steady~1"]);
    let ahead_one = preview_remove_worktree(&repo, "steady").expect("preview one ahead");
    assert!(
        matches!(ahead_one.merge, MergeState::NotMerged { .. }),
        "{:?}",
        ahead_one.merge
    );
    assert_eq!(ahead_one.unmerged_commits, Some(1));
    assert_ne!(
        ahead_one.token, ahead_two.token,
        "one commit fewer moves the token"
    );

    // Another branch holds the same commit: the answer and the count stay and
    // the base named in the label changes.
    git(&repo, &["branch", "other-base", "worktree-steady~1"]);
    write_managed_worktree(
        &repo,
        &ManagedWorktree::new("steady", Some("other-base".to_string())),
    )
    .expect("record against another base");
    let relabelled = preview_remove_worktree(&repo, "steady").expect("preview other base");
    assert!(
        matches!(relabelled.merge, MergeState::NotMerged { .. }),
        "{:?}",
        relabelled.merge
    );
    assert_eq!(relabelled.unmerged_commits, Some(1));
    assert_ne!(
        relabelled.token, ahead_one.token,
        "the base the question was asked against moves the token"
    );

    // No base recorded leaves the question unasked, which is not the state of
    // an answered question with no work between.
    write_managed_worktree(&repo, &ManagedWorktree::new("steady", None))
        .expect("record without a base");
    let unknown = preview_remove_worktree(&repo, "steady").expect("preview without a base");
    assert_eq!(unknown.merge, MergeState::Unknown);
    assert_ne!(
        unknown.token, relabelled.token,
        "the merge answer moves the token"
    );
    drop(remove_dir_all(&repo));
}

/// The token names the holds on a tree: a session's lease and Git's own lock
/// both refuse a removal, and either one is part of the state the user was
/// shown.
#[test]
fn test_holds_move_token() {
    let repo = make_repo("holds");
    let path = managed(&repo, "busy");
    let free = preview_remove_worktree(&repo, "busy").expect("preview");
    assert!(!free.leased, "nobody leased the tree");
    assert!(!free.locked, "nobody locked the tree");

    let lease = WorktreeLease::acquire(&repo, "busy").expect("take the lease");
    let leased = preview_remove_worktree(&repo, "busy").expect("preview while leased");
    assert!(leased.leased, "the lease is reported");
    assert_ne!(leased.token, free.token, "a lease moves the token");
    drop(lease);
    let released = preview_remove_worktree(&repo, "busy").expect("preview after release");
    assert_eq!(
        released.token, free.token,
        "a released lease restores the state"
    );

    git(&repo, &["worktree", "lock", &path.to_string_lossy()]);
    let locked = preview_remove_worktree(&repo, "busy").expect("preview while locked");
    assert!(locked.locked, "the lock is reported");
    assert_ne!(locked.token, released.token, "a lock moves the token");
    git(&repo, &["worktree", "unlock", &path.to_string_lossy()]);
    let unlocked = preview_remove_worktree(&repo, "busy").expect("preview after unlock");
    assert_eq!(
        unlocked.token, free.token,
        "clearing the lock restores the state"
    );
    drop(remove_dir_all(&repo));
}

/// Each change class moves the token on its own. The counts are part of the
/// state the user was shown, so a preview after one more file of any class is
/// a different state rather than the same decision.
#[test]
fn test_change_classes_move_token() {
    let repo = make_repo("classes-token");
    let path = managed(&repo, "classes");
    write(path.join(".gitignore"), "target/\n.env.local\n").expect("write ignore rules");
    write(path.join("tracked.txt"), "one\n").expect("write the tracked file");
    git(&path, &["add", ".gitignore", "tracked.txt"]);
    git(&path, &["commit", "-m", "seed", "-q"]);
    let settled = preview_remove_worktree(&repo, "classes").expect("preview");
    assert_eq!(settled.changes.tracked_dirty, 0, "{:?}", settled.changes);

    write(path.join("tracked.txt"), "two\n").expect("edit the tracked file");
    let tracked = preview_remove_worktree(&repo, "classes").expect("preview after the edit");
    assert_eq!(tracked.changes.tracked_dirty, 1);
    assert_ne!(
        tracked.token, settled.token,
        "a tracked edit moves the token"
    );

    write(path.join("untracked.txt"), "new\n").expect("write an untracked file");
    let untracked = preview_remove_worktree(&repo, "classes").expect("preview after the file");
    assert_eq!(untracked.changes.untracked, 1);
    assert_ne!(
        untracked.token, tracked.token,
        "an untracked path moves the token"
    );

    create_dir_all(path.join("target").join("debug")).expect("create the build directory");
    write(path.join("target").join("debug").join("out"), "bin").expect("fill the build directory");
    let build = preview_remove_worktree(&repo, "classes").expect("preview after the build");
    assert_eq!(build.changes.ignored_build_dirs, vec!["target".to_string()]);
    assert_ne!(
        build.token, untracked.token,
        "a build directory moves the token"
    );

    write(path.join(".env.local"), "secret\n").expect("write the ignored file");
    let hidden = preview_remove_worktree(&repo, "classes").expect("preview after the ignored file");
    assert_eq!(hidden.changes.ignored_unknown, 1);
    assert_ne!(
        hidden.token, build.token,
        "an ignored entry moves the token"
    );
    drop(remove_dir_all(&repo));
}

/// Two states holding the same bytes split differently across two fields do
/// not share a digest: the separator is a byte no path or branch name can
/// hold, so content cannot move across a field boundary.
#[test]
fn test_token_separates_fields() {
    let changes = WorktreeChanges::default();
    let late = WorktreeIdentity {
        path: PathBuf::from("/w"),
        common_dir: PathBuf::from("/y"),
        branch: "z\nb".to_string(),
        head: "1".to_string(),
        locked: false,
    };
    let early = WorktreeIdentity {
        common_dir: PathBuf::from("/y\nz"),
        branch: "b".to_string(),
        ..late.clone()
    };

    let late_token = preview_token(&late, &changes, &MergeState::Unknown, None, false, false);
    let early_token = preview_token(&early, &changes, &MergeState::Unknown, None, false, false);
    assert_ne!(
        early_token, late_token,
        "a value cannot be read as the end of one field and the start of the next"
    );
}

/// A worktree this product did not record is not previewed: removal must not
/// act on a directory whose origin is unknown.
#[test]
fn test_preview_requires_record() {
    let repo = make_repo("unmanaged");
    unmanaged(&repo, "stranger");

    let preview = preview_remove_worktree(&repo, "stranger");
    assert!(
        matches!(preview, Err(WorktreeError::Unmanaged { .. })),
        "{preview:?}"
    );
    drop(remove_dir_all(&repo));
}

/// A record this version cannot read is refused as unusable rather than as a
/// tree with no record: the two states call for different repairs, and only
/// one of them is a directory this product never made.
#[test]
fn test_preview_rejects_unusable_record() {
    let repo = make_repo("unusable");
    unmanaged(&repo, "damaged");
    let path = record_path(&repo, "damaged");
    create_dir_all(path.parent().expect("state dir")).expect("create state dir");
    write(&path, "{not json").expect("write bytes");

    let preview = preview_remove_worktree(&repo, "damaged");
    assert!(
        matches!(preview, Err(WorktreeError::RecordRejected { .. })),
        "{preview:?}"
    );
    drop(remove_dir_all(&repo));
}

/// A directory at the managed path that belongs to another repository fails
/// the identity check even when a record claims it.
#[test]
fn test_preview_rejects_foreign_repo() {
    let home = make_repo("home");
    let other = make_repo("other");
    let path = worktree_path_for(&home, "shared");
    create_dir_all(path.parent().expect("managed dir")).expect("create managed dir");
    git(
        &other,
        &[
            "worktree",
            "add",
            "-b",
            "worktree-shared",
            &path.to_string_lossy(),
            "HEAD",
        ],
    );
    write_managed_worktree(&home, &ManagedWorktree::new("shared", None)).expect("record");

    let preview = preview_remove_worktree(&home, "shared");
    assert!(
        matches!(preview, Err(WorktreeError::Identity { .. })),
        "{preview:?}"
    );
    drop(remove_dir_all(&home));
    drop(remove_dir_all(&other));
}

/// A link planted at the managed path resolves elsewhere, which the managed
/// root check refuses rather than following to the target.
#[cfg(unix)]
#[test]
fn test_preview_rejects_planted_link() {
    let repo = make_repo("link");
    let path = managed(&repo, "linked");
    let outside = repo.join("outside");
    rename(&path, &outside).expect("move the tree aside");
    symlink(&outside, &path).expect("plant the link");

    let preview = preview_remove_worktree(&repo, "linked");
    assert!(
        matches!(preview, Err(WorktreeError::Identity { .. })),
        "{preview:?}"
    );
    drop(remove_dir_all(&repo));
}

/// A link at the managed path pointing at a worktree of this same repository,
/// on the expected branch and named in Git's own listing, is refused on its
/// location alone: the managed directory is where this product creates trees,
/// and a path resolving outside it is not the object the slug names.
#[cfg(unix)]
#[test]
fn test_preview_rejects_link_outside() {
    let repo = make_repo("link-root");
    let slug = "linked";
    let outside = repo.join("linked");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            &worktree_branch_name(slug),
            &outside.to_string_lossy(),
            "HEAD",
        ],
    );
    let path = worktree_path_for(&repo, slug);
    create_dir_all(path.parent().expect("managed dir")).expect("create the managed directory");
    symlink(&outside, &path).expect("plant the link");
    write_managed_worktree(&repo, &ManagedWorktree::new(slug, None)).expect("record");

    let preview = preview_remove_worktree(&repo, slug);
    assert!(
        matches!(preview, Err(WorktreeError::Identity { .. })),
        "a listed worktree of this repository is refused for its location: {preview:?}"
    );
    drop(remove_dir_all(&repo));
}

/// A worktree checked out on another branch is refused: the record names the
/// branch the removal would coordinate, and a tree on a different one is not
/// the object the record describes.
#[test]
fn test_preview_rejects_other_branch() {
    let repo = make_repo("branch");
    let path = managed(&repo, "switched");
    git(&path, &["checkout", "-q", "-b", "somewhere-else"]);

    let preview = preview_remove_worktree(&repo, "switched");
    assert!(
        matches!(preview, Err(WorktreeError::Identity { .. })),
        "{preview:?}"
    );
    drop(remove_dir_all(&repo));
}

/// A directory that presents the repository's Git directory but is not a
/// worktree Git lists is refused. The record's branch and the repository's own
/// branch are made to agree, so only Git's record can tell the two apart.
#[test]
fn test_preview_rejects_unlisted_tree() {
    let repo = make_repo("unlisted");
    let slug = "copied";
    let branch = worktree_branch_name(slug);
    git(&repo, &["checkout", "-q", "-b", &branch]);
    let path = worktree_path_for(&repo, slug);
    create_dir_all(&path).expect("plant the directory");
    write(
        path.join(".git"),
        format!("gitdir: {}\n", repo.join(".git").display()),
    )
    .expect("point the directory at the repository's Git directory");
    write_managed_worktree(&repo, &ManagedWorktree::new(slug, None)).expect("record");

    let preview = preview_remove_worktree(&repo, slug);
    assert!(
        matches!(preview, Err(WorktreeError::Identity { .. })),
        "a directory Git does not list is refused even when the record claims it: {preview:?}"
    );
    drop(remove_dir_all(&repo));
}
