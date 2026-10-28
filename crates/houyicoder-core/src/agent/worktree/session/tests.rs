#![expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]

use super::super::preview::MergeState;
use super::super::preview::preview_remove_worktree;
use super::super::record::{WorktreeLease, read_managed_worktree, record_path, state_dir};
use super::*;
use std::env::temp_dir;
use std::fs;
use std::process::id;

mod removal;

/// A throwaway git repo with one commit, so a remove test runs its body
/// deterministically instead of skipping outside a live work tree.
fn make_repo(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-wts-{tag}-{}", id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).expect("repo dir");
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .status(),
    );
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["config", "user.email", "t@x"])
            .status(),
    );
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["config", "user.name", "t"])
            .status(),
    );
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["commit", "--allow-empty", "-m", "init", "-q"])
            .status(),
    );
    dir
}

#[test]
fn test_validates_legal_slugs() {
    assert!(validate_worktree_slug("feat-x").is_ok());
    assert!(validate_worktree_slug("user/feature").is_ok());
    assert!(validate_worktree_slug("a.b_c-d").is_ok());
}

#[test]
fn test_rejects_bad_slugs() {
    assert!(validate_worktree_slug("").is_err(), "empty");
    assert!(validate_worktree_slug(".").is_err(), "dot");
    assert!(validate_worktree_slug("..").is_err(), "dotdot");
    assert!(validate_worktree_slug("a/../b").is_err(), "traversal");
    assert!(validate_worktree_slug("/leading").is_err(), "leading slash");
    assert!(
        validate_worktree_slug("trailing/").is_err(),
        "trailing slash"
    );
    assert!(validate_worktree_slug("a//b").is_err(), "empty segment");
    assert!(validate_worktree_slug("bad space").is_err(), "space");
    assert!(validate_worktree_slug("bad:colon").is_err(), "colon");
    assert!(validate_worktree_slug(&"x".repeat(65)).is_err(), "too long");
}

#[test]
fn test_flatten_replaces_slash() {
    assert_eq!(flatten_slug("user/feature"), "user+feature");
    assert_eq!(flatten_slug("plain"), "plain");
    assert_eq!(flatten_slug("a/b/c"), "a+b+c");
}

/// A scratch directory that is not a git repository, for the path checks
/// that read the filesystem and run no git.
fn scratch(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-wtspath-{tag}-{}", id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The directory a managed worktree sits in names its slug again, including
/// one whose slug carries a slash.
#[test]
fn test_managed_slug_round_trips() {
    let root = scratch("inverts");
    let managed = worktrees_dir(&root);
    fs::create_dir_all(managed.join("feat+x")).expect("managed dir");
    fs::create_dir_all(managed.join("plain")).expect("managed dir");
    assert_eq!(
        managed_slug_for_path(&root, &managed.join("feat+x")).expect("a managed tree"),
        "feat/x"
    );
    assert_eq!(
        managed_slug_for_path(&root, &managed.join("plain")).expect("a managed tree"),
        "plain"
    );
}

/// A directory this product did not create answers unmanaged, so a worktree
/// Git lists but no record names cannot be entered, previewed, or removed.
#[test]
fn test_managed_slug_refuses_foreign() {
    let root = scratch("foreign");
    let managed = worktrees_dir(&root);
    fs::create_dir_all(managed.join("inside").join("nested")).expect("nested dir");
    fs::create_dir_all(root.join("elsewhere")).expect("foreign dir");
    let foreign = [
        root.join("elsewhere"),
        managed.join("inside").join("nested"),
        managed.join("missing"),
    ];
    for path in foreign {
        assert!(
            matches!(
                managed_slug_for_path(&root, &path),
                Err(WorktreeError::UnmanagedPath { .. })
            ),
            "{path:?} is not a managed worktree"
        );
    }
}

#[test]
fn test_branch_name_uses_slug() {
    assert_eq!(
        worktree_branch_name("user/feature"),
        "worktree-user+feature"
    );
    let repo = Path::new("/repo");
    assert_eq!(
        worktree_path_for(repo, "user/feature"),
        Path::new("/repo/.houyicoder/worktrees/user+feature")
    );
}

/// Smoke test against a real throwaway worktree of the host repo: create
/// then content visible then resume same path then cleanup. Skipped when
/// not run from a git work tree.
#[test]
fn test_get_create_round_trips() {
    let cwd = current_dir().expect("cwd");
    if !cwd.join(".git").exists() {
        return;
    }
    let slug = "smoke-round-trip";
    drop(cleanup_worktree(&cwd, slug));
    let r = get_or_create_worktree(&cwd, slug).expect("create");
    assert!(!r.existed, "first call creates");
    assert!(
        r.worktree_path.join("Cargo.toml").exists(),
        "worktree sees repo content"
    );
    assert_eq!(r.worktree_branch, "worktree-smoke-round-trip");
    let r2 = get_or_create_worktree(&cwd, slug).expect("resume");
    assert!(r2.existed, "second call resumes");
    assert_eq!(r.worktree_path, r2.worktree_path);
    drop(cleanup_worktree(&cwd, slug));
    assert!(
        !r.worktree_path.exists(),
        "worktree dir removed after cleanup"
    );
}

/// A tree that exists without a record — made by another tool, or left by a
/// creation whose record was lost — is adopted on resume, so a removal later
/// has the object its gate acts on instead of refusing a tree nobody can
/// clean up.
#[test]
fn test_resume_adopts_record() {
    let repo = make_repo("adopt");
    let slug = "adopted";
    let path = worktree_path_for(&repo, slug);
    fs::create_dir_all(path.parent().expect("managed dir")).expect("create dir");
    let branch = worktree_branch_name(slug);
    let added = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "worktree",
            "add",
            "-b",
            &branch,
            &path.to_string_lossy(),
            "HEAD",
        ])
        .status()
        .expect("git worktree add");
    assert!(added.success(), "the tree exists without a record");
    assert!(
        read_managed_worktree(&repo, slug).is_none(),
        "no record yet"
    );

    let created = get_or_create_worktree(&repo, slug).expect("resume");
    assert!(created.existed, "the tree is resumed, not recreated");
    let record = read_managed_worktree(&repo, slug).expect("adopted record");
    assert_eq!(record.slug(), slug);
    assert_eq!(
        record.base_branch(),
        None,
        "a fork point nobody saw is not filled in with the branch checked out today"
    );
    let preview = preview_remove_worktree(&repo, slug).expect("preview the adopted tree");
    assert_eq!(
        preview.merge,
        MergeState::Unknown { base: None },
        "an adopted tree's merge answer is unknown, not merged"
    );
    assert_eq!(preview.unmerged_commits, None);
    drop(fs::remove_dir_all(&repo));
}

/// A tree checked out on another branch is refused, not resumed: the removal
/// preview would refuse it too, so a session started in it could never be
/// removed from that path.
#[test]
fn test_resume_refuses_foreign_branch() {
    let repo = make_repo("foreign");
    let slug = "foreign";
    let path = worktree_path_for(&repo, slug);
    fs::create_dir_all(path.parent().expect("managed dir")).expect("create dir");
    let added = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "worktree",
            "add",
            "-b",
            "other-branch",
            &path.to_string_lossy(),
            "HEAD",
        ])
        .status()
        .expect("git worktree add");
    assert!(added.success(), "the tree exists on another branch");

    let error = get_or_create_worktree(&repo, slug).expect_err("a foreign branch is refused");
    assert!(
        matches!(error, WorktreeError::Identity { .. }),
        "expected Identity, got {error:?}"
    );
    assert!(
        read_managed_worktree(&repo, slug).is_none(),
        "no record is written for a tree on another branch"
    );
    assert!(path.exists(), "the refused tree is left untouched");
    drop(fs::remove_dir_all(&repo));
}

/// A tree Git has locked is not resumed into: the session would work in a tree
/// that no removal accepts until someone releases the lock.
#[test]
fn test_resume_refuses_locked() {
    let repo = make_repo("resume-locked");
    let slug = "resume-locked";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let path = created.worktree_path.to_string_lossy().into_owned();
    run_git(&repo, &["worktree", "lock", &path]).expect("lock the tree");

    let error = get_or_create_worktree(&repo, slug).expect_err("a locked tree is refused");
    assert!(
        matches!(error, WorktreeError::Locked { .. }),
        "expected Locked, got {error:?}"
    );
    drop(run_git(&repo, &["worktree", "unlock", &path]));
    drop(cleanup_worktree(&repo, slug));
    drop(fs::remove_dir_all(&repo));
}

/// A tree that belongs to another repository is refused: the common directory
/// is what makes a worktree this repository's, and a slug only names a path.
#[test]
fn test_resume_refuses_foreign_repo() {
    let home = make_repo("home-resume");
    let other = make_repo("other-resume");
    let slug = "shared";
    let path = worktree_path_for(&home, slug);
    fs::create_dir_all(path.parent().expect("managed dir")).expect("create dir");
    let branch = worktree_branch_name(slug);
    let added = Command::new("git")
        .arg("-C")
        .arg(&other)
        .args([
            "worktree",
            "add",
            "-b",
            &branch,
            &path.to_string_lossy(),
            "HEAD",
        ])
        .status()
        .expect("git worktree add");
    assert!(added.success(), "the other repository owns the path");

    let error = get_or_create_worktree(&home, slug).expect_err("a foreign tree is refused");
    assert!(
        matches!(error, WorktreeError::Identity { .. }),
        "expected Identity, got {error:?}"
    );
    assert!(
        read_managed_worktree(&home, slug).is_none(),
        "no record is written for another repository's tree"
    );
    drop(fs::remove_dir_all(&home));
    drop(fs::remove_dir_all(&other));
}

/// A record this version refused stops a resume outright, and survives byte
/// for byte: it is the only evidence of what the tree is, so a tree read
/// through a file that could not be read is not resumed.
#[test]
fn test_resume_refuses_rejected_record() {
    let repo = make_repo("rejected");
    let slug = "rejected";
    let path = worktree_path_for(&repo, slug);
    fs::create_dir_all(path.parent().expect("managed dir")).expect("create dir");
    let branch = worktree_branch_name(slug);
    let added = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "worktree",
            "add",
            "-b",
            &branch,
            &path.to_string_lossy(),
            "HEAD",
        ])
        .status()
        .expect("git worktree add");
    assert!(added.success(), "the tree exists");
    let record = record_path(&repo, slug);
    fs::create_dir_all(record.parent().expect("state dir")).expect("create state dir");
    let newer = r#"{"schema":9,"slug":"rejected","branch":"worktree-rejected","base_branch":null,"created_ms":1}"#;
    fs::write(&record, newer).expect("write a record this version refuses");

    let error = get_or_create_worktree(&repo, slug).expect_err("a refused record stops the resume");
    assert!(
        matches!(error, WorktreeError::RecordRejected { .. }),
        "expected RecordRejected, got {error:?}"
    );
    assert_eq!(
        fs::read_to_string(&record).expect("read the record"),
        newer,
        "the refused record is not overwritten"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A removal leaves no record or lease behind: the state directory would
/// otherwise grow one dead entry per worktree this product ever removed.
#[test]
fn test_removal_clears_worktree_state() {
    let repo = make_repo("residue");
    let slug = "residue";
    get_or_create_worktree(&repo, slug).expect("create");
    let lease = WorktreeLease::acquire(&repo, slug).expect("lease");
    drop(lease);
    assert!(record_path(&repo, slug).exists(), "the record was written");

    remove_clean_worktree(&repo, slug).expect("remove");
    let left = state_files(&repo);
    assert!(left.is_empty(), "no record or lease left: {left:?}");
    drop(fs::remove_dir_all(&repo));
}

/// The user removal path leaves no record or lease behind either: both paths
/// end in the same clear, so the state directory does not grow one dead entry
/// per removed worktree whichever one ran.
#[test]
fn test_user_removal_clears_state() {
    let repo = make_repo("user-residue");
    let slug = "user-residue";
    get_or_create_worktree(&repo, slug).expect("create");

    remove_worktree(&repo, slug).expect("remove");

    let left = state_files(&repo);
    assert!(left.is_empty(), "no record or lease left: {left:?}");
    drop(fs::remove_dir_all(&repo));
}

/// The files a removal leaves in the state directory. Subdirectories are not
/// entries to clean up: the cleanup records keep a directory of their own,
/// created once and shared by every slug.
fn state_files(repo_root: &Path) -> Vec<PathBuf> {
    fs::read_dir(state_dir(repo_root))
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().is_file())
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default()
}

/// A removal Git refused keeps the directory, its branch, and its record: a
/// record erased here would leave a directory no later removal accepts, and a
/// deleted branch would take work the refused step never discarded.
#[test]
fn test_failed_removal_keeps_record() {
    let repo = make_repo("keeps-record");
    let slug = "held-tree";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    fs::write(created.worktree_path.join("draft.txt"), "work\n").expect("write work");

    let error = remove_clean_worktree(&repo, slug).expect_err("a dirty tree is not removed");

    assert!(matches!(error, WorktreeError::Git { .. }), "{error:?}");
    assert!(created.worktree_path.exists(), "the directory survives");
    assert!(
        read_managed_worktree(&repo, slug).is_some(),
        "the record survives so a later removal finds its object"
    );
    let branch = worktree_branch_name(slug);
    assert!(
        run_git(
            &repo,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")]
        )
        .is_ok(),
        "the branch survives with the directory"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A creation whose record cannot be published removes the directory it just
/// made: a tree with no record is residue no removal accepts, so the act
/// either leaves both or neither.
#[test]
fn test_unrecorded_create_rolls_back() {
    let repo = make_repo("rollback");
    let slug = "unrecorded";
    let blocked = record_path(&repo, slug);
    fs::create_dir_all(&blocked).expect("occupy the record path");
    // The caller holds the slug's lease, as the enter path does before it
    // creates anything: the rollback must undo its own tree through that, or
    // the refusal leaves a directory no record describes.
    let lease = WorktreeLease::acquire(&repo, slug).expect("hold the lease");

    let error = get_or_create_worktree(&repo, slug).expect_err("the record cannot be published");
    assert!(
        matches!(error, WorktreeError::State { .. }),
        "a record that cannot be stored is a state failure, not a git one: {error:?}"
    );
    assert!(
        !worktree_path_for(&repo, slug).exists(),
        "the directory is rolled back with the record"
    );
    let branch = worktree_branch_name(slug);
    let branch_ref = format!("refs/heads/{branch}");
    assert!(
        run_git(&repo, &["rev-parse", "--verify", &branch_ref]).is_err(),
        "the branch is rolled back with the directory"
    );
    drop(lease);
    drop(fs::remove_dir_all(&repo));
}

/// A record this version refuses stops a removal before Git is asked: nothing
/// establishes that the directory is the tree the record describes, so the
/// directory and its branch are left alone.
#[test]
fn test_remove_refuses_other_schema() {
    let repo = make_repo("remove-schema");
    let slug = "future-tree";
    let path = worktree_path_for(&repo, slug);
    fs::create_dir_all(&path).expect("managed path");
    let record = record_path(&repo, slug);
    fs::create_dir_all(record.parent().expect("state dir")).expect("state dir");
    fs::write(
        &record,
        r#"{"schema":2,"slug":"future-tree","branch":"worktree-future-tree","base_branch":null,"created_ms":1}"#,
    )
    .expect("write a record from a later version");

    let error = remove_clean_worktree(&repo, slug).expect_err("the record is refused");
    assert!(
        matches!(error, WorktreeError::RecordRejected { .. }),
        "expected RecordRejected, got {error:?}"
    );
    assert!(path.exists(), "the directory is left alone");
    drop(fs::remove_dir_all(&repo));
}

/// A branch Git will not delete once the directory is gone answers with what
/// survived rather than with Git's message about the branch: the directory is
/// removed, and the branch and its record stay, because a branch holding work
/// is never deleted to make an act look finished.
#[test]
fn test_branch_delete_failure_reported() {
    let repo = make_repo("branch-refused");
    let slug = "unmerged-tree";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    fs::write(created.worktree_path.join("work.txt"), "work\n").expect("write work");
    run_git(&created.worktree_path, &["add", "work.txt"]).expect("stage the work");
    run_git(&created.worktree_path, &["commit", "-m", "work", "-q"]).expect("commit the work");

    let error = remove_clean_worktree(&repo, slug).expect_err("an unmerged branch is not deleted");
    assert!(
        matches!(error, WorktreeError::RemovalIncomplete { .. }),
        "expected RemovalIncomplete, got {error:?}"
    );
    assert!(!created.worktree_path.exists(), "the directory is gone");
    let branch = worktree_branch_name(slug);
    assert!(
        run_git(
            &repo,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")]
        )
        .is_ok(),
        "the branch survives with the work it holds"
    );
    assert!(
        read_managed_worktree(&repo, slug).is_some(),
        "the record survives beside the branch it names"
    );
    drop(fs::remove_dir_all(&repo));
}

/// An ignored file no build name covers counts as work: nothing can tell what
/// it is, and a gate that read it as regenerable output would let a removal
/// discard a file the user's own rules hide from Git.
#[test]
fn test_gate_counts_hidden_file() {
    let repo = make_repo("hidden-file");
    let slug = "hidden-work";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let path = created.worktree_path;
    fs::write(path.join(".gitignore"), ".env.local\n").expect("write ignore rules");
    run_git(&path, &["add", ".gitignore"]).expect("stage the ignore rules");
    run_git(&path, &["commit", "-m", "ignore rules", "-q"]).expect("commit the ignore rules");
    let baseline = run_git(&path, &["rev-parse", "HEAD"]).expect("baseline commit");
    assert!(
        !has_worktree_changes(&path, &baseline),
        "a tree whose only commit is the baseline holds no work"
    );

    fs::write(path.join(".env.local"), "secret\n").expect("write the ignored file");
    assert!(
        has_worktree_changes(&path, &baseline),
        "an ignored file no build name covers is work"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A known build directory does not count: it is regenerable output, and
/// counting it would mark every tree a build ever ran in as holding work.
#[test]
fn test_gate_allows_build_output() {
    let repo = make_repo("build-output");
    let slug = "built-tree";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let path = created.worktree_path;
    fs::write(path.join(".gitignore"), "target/\n").expect("write ignore rules");
    run_git(&path, &["add", ".gitignore"]).expect("stage the ignore rules");
    run_git(&path, &["commit", "-m", "ignore rules", "-q"]).expect("commit the ignore rules");
    let baseline = run_git(&path, &["rev-parse", "HEAD"]).expect("baseline commit");

    fs::create_dir_all(path.join("target").join("debug")).expect("create the build directory");
    fs::write(path.join("target").join("debug").join("out"), "bin").expect("fill the build output");
    assert!(
        !has_worktree_changes(&path, &baseline),
        "build output a known name covers is not work"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A pre-existing same-named branch makes the -b add collide (the
/// worktree dir does not yet exist, so the fast-resume path is skipped).
#[test]
fn test_collision_on_existing_branch() {
    let cwd = current_dir().expect("cwd");
    if !cwd.join(".git").exists() {
        return;
    }
    let slug = "smoke-collision";
    drop(cleanup_worktree(&cwd, slug));
    let branch = worktree_branch_name(slug);
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .args(["branch", &branch])
            .status(),
    );
    let err = get_or_create_worktree(&cwd, slug).expect_err("collision");
    assert!(
        matches!(err, WorktreeError::Collision { .. }),
        "expected Collision, got {err:?}"
    );
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .args(["branch", "-D", &branch])
            .status(),
    );
}

/// Fail-closed: count_worktree_changes returns None when there is no
/// baseline commit (cannot prove clean, assume unsafe).
#[test]
fn test_count_changes_without_baseline() {
    assert!(count_worktree_changes(Path::new("/nonexistent"), None).is_none());
}

/// The conservative remove deletes both the tree dir and its branch after
/// a clean create: no --force needed, and branch -d accepts a branch that
/// has no unmerged work.
#[test]
fn test_remove_clean_removes_tree() {
    let repo = make_repo("removes-tree");
    let slug = "clean-remove";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    assert!(created.worktree_path.exists(), "tree created");
    remove_clean_worktree(&repo, slug).expect("clean remove");
    assert!(!created.worktree_path.exists(), "tree dir removed");
    let branch = worktree_branch_name(slug);
    assert!(
        run_git(&repo, &["rev-parse", "--verify", &branch]).is_err(),
        "branch deleted with the tree"
    );
    drop(fs::remove_dir_all(&repo));
}

/// Removal refuses a tree another session holds, so a session's own release
/// is what lets its exit through and an outsider's removal waits its turn.
#[test]
fn test_remove_refuses_leased() {
    let repo = make_repo("refuses-leased");
    let slug = "busy-remove";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let lease = WorktreeLease::acquire(&repo, slug).expect("take the lease");

    let err = remove_clean_worktree(&repo, slug).expect_err("a leased tree is refused");
    assert!(
        matches!(err, WorktreeError::Leased { .. }),
        "expected Leased, got {err:?}"
    );
    assert!(
        created.worktree_path.exists(),
        "the refused removal deleted nothing"
    );

    drop(lease);
    remove_clean_worktree(&repo, slug).expect("the released tree is removed");
    assert!(!created.worktree_path.exists(), "tree dir removed");
    drop(fs::remove_dir_all(&repo));
}

/// A directory at the managed path that this product never recorded is left
/// alone even though Git lists it: being a Git worktree does not make it an
/// object this product may remove.
#[test]
fn test_remove_refuses_foreign_tree() {
    let repo = make_repo("foreign-tree");
    let slug = "planted-tree";
    let path = worktree_path_for(&repo, slug);
    fs::create_dir_all(path.parent().expect("managed root")).expect("managed root");
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            &worktree_branch_name(slug),
            &path.to_string_lossy(),
            "HEAD",
        ],
    )
    .expect("plant a tree Git lists");

    let err = remove_clean_worktree(&repo, slug).expect_err("an unrecorded tree is refused");
    assert!(
        matches!(err, WorktreeError::Unmanaged { .. }),
        "unmanaged refusal: {err:?}"
    );
    assert!(path.exists(), "the tree is left alone");
    drop(cleanup_worktree(&repo, slug));
    drop(fs::remove_dir_all(&repo));
}

/// A tree Git has locked is refused with that reason before anything is
/// removed, because overriding a lock takes a second force that is not a
/// general fallback.
#[test]
fn test_remove_refuses_locked() {
    let repo = make_repo("locked");
    let slug = "locked-tree";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    let path = created.worktree_path.to_string_lossy().into_owned();
    run_git(&repo, &["worktree", "lock", &path]).expect("lock the tree");

    let err = remove_worktree(&repo, slug).expect_err("a locked tree is refused");
    assert!(
        matches!(err, WorktreeError::Locked { .. }),
        "locked refusal: {err:?}"
    );
    assert!(created.worktree_path.exists(), "the tree is left alone");
    drop(run_git(&repo, &["worktree", "unlock", &path]));
    drop(cleanup_worktree(&repo, slug));
    drop(fs::remove_dir_all(&repo));
}

/// A refusal is the sentence the user acts on, so each one names what failed
/// and what survived. A variant that printed only a path would leave the user
/// with no reason and no next step.
#[test]
fn test_errors_name_the_failure() {
    let path = PathBuf::from("/repo/.houyicoder/worktrees/tree");
    let cases = [
        (
            WorktreeError::Unmanaged {
                slug: "tree".into(),
            },
            "carries no creation record",
        ),
        (
            WorktreeError::RecordRejected {
                slug: "tree".into(),
                reason: "it could not be parsed".into(),
            },
            "cannot use (it could not be parsed)",
        ),
        (
            WorktreeError::Identity {
                path: path.clone(),
                reason: "it is on another branch".into(),
            },
            "is not the worktree this slug names: it is on another branch",
        ),
        (
            WorktreeError::Locked {
                path: path.clone(),
                reason: "Git records a lock on it".into(),
            },
            "is locked: Git records a lock on it",
        ),
        (
            WorktreeError::State {
                path: path.clone(),
                reason: "the directory is read-only".into(),
            },
            "could not be used: the directory is read-only",
        ),
        (
            WorktreeError::Leased {
                slug: "tree".into(),
                reason: "another session holds it".into(),
            },
            "is in use: another session holds it",
        ),
        (
            WorktreeError::LeaseUnknown {
                slug: "tree".into(),
                reason: "the lease file is a directory".into(),
            },
            "in-use state unknown: the lease file is a directory",
        ),
        (
            WorktreeError::RemovalIncomplete {
                path: path.clone(),
                branch: "worktree-tree".into(),
                reason: "it holds unmerged commits".into(),
            },
            "was removed, but branch worktree-tree could not be deleted",
        ),
        (
            WorktreeError::CleanupPending {
                path: path.clone(),
                reason: "a file inside it is in use".into(),
            },
            "is no longer registered as a worktree, but its directory could not be removed: a file inside it is in use",
        ),
    ];
    for (error, expected) in cases {
        let shown = error.to_string();
        assert!(
            shown.contains(expected),
            "missing {expected:?} in {shown:?}"
        );
    }
}

fn cleanup_worktree(repo_root: &Path, slug: &str) -> Result<(), WorktreeError> {
    let path = worktree_path_for(repo_root, slug);
    let branch = worktree_branch_name(slug);
    drop(run_git(
        repo_root,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    ));
    drop(run_git(repo_root, &["branch", "-D", &branch]));
    Ok(())
}
