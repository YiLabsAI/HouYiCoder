#![expect(
    clippy::disallowed_methods,
    reason = "test setup spawn, not model-driven"
)]

use super::*;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;

/// The slug tests need a real repo, so they skip when git is absent.
fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// A repo whose dir name is exactly the given name, under a pid- and
/// seq-unique parent so parallel runs do not collide. One empty commit, so a
/// linked worktree has a HEAD to branch from.
fn temp_git_repo(name: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let parent = temp_dir().join(format!("houyi-slug-{}-{}", id(), seq));
    drop(remove_dir_all(&parent));
    create_dir_all(&parent).expect("mkdir parent");
    let dir = parent.join(name);
    create_dir_all(&dir).expect("mkdir repo");
    Command::new("git")
        .arg("-C")
        .arg(&dir)
        .arg("init")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git init");
    Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-m",
            "x",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git commit");
    dir
}

/// None when git refuses, so the caller skips rather than fails.
fn add_linked_worktree(main: &Path, name: &str) -> Option<PathBuf> {
    let wt = main
        .parent()
        .expect("parent")
        .join(format!("{name}-{}", id()));
    let added = Command::new("git")
        .arg("-C")
        .arg(main)
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if added { Some(wt) } else { None }
}

/// Remove the linked worktree and the whole temp parent dir.
fn cleanup(main: &Path, wt: &Path) {
    drop(
        Command::new("git")
            .arg("-C")
            .arg(main)
            .args(["worktree", "remove", "--force"])
            .arg(wt)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    );
    drop(remove_dir_all(main.parent().expect("parent")));
}

/// A main repo's slug is its own dir name (the git common dir's parent).
#[test]
fn test_slug_matches_repo_name() {
    if !git_available() {
        return;
    }
    let repo = temp_git_repo("mainrepo");
    assert_eq!(git_canonical_slug(&repo), "mainrepo");
    drop(remove_dir_all(repo.parent().expect("parent")));
}

/// A linked worktree must resolve to the same slug as its main repo so a
/// project keeps one memory dir. Deriving the slug from the workspace dir
/// name would split memory across every worktree the user creates.
#[test]
fn test_linked_shares_slug() {
    if !git_available() {
        return;
    }
    let main = temp_git_repo("sharedmain");
    let Some(wt) = add_linked_worktree(&main, "sharedwt") else {
        drop(remove_dir_all(main.parent().expect("parent")));
        return;
    };
    let main_slug = git_canonical_slug(&main);
    assert_eq!(
        main_slug,
        git_canonical_slug(&wt),
        "worktree must share the main repo slug"
    );
    assert_eq!(main_slug, "sharedmain");
    cleanup(&main, &wt);
}

/// A linked worktree's .git is a gitfile, not a directory. The fence must
/// allow-back the real shared .git, so the lookup follows that indirection;
/// allow-backing the gitfile would leave git commands failing in the
/// worktree session.
#[test]
fn test_common_dir_resolves_gitfile() {
    if !git_available() {
        return;
    }
    let main = temp_git_repo("sharedmain2");
    let Some(wt) = add_linked_worktree(&main, "sharedwt2") else {
        drop(remove_dir_all(main.parent().expect("parent")));
        return;
    };
    let main_git = canonicalize(main.join(".git")).expect("canonicalize main .git");
    let wt_common = git_common_dir(&wt).expect("common dir resolves for linked worktree");
    assert_eq!(
        canonicalize(&wt_common).expect("canonicalize wt common"),
        main_git,
        "linked worktree common dir must be the main repo .git, not the gitfile"
    );
    assert!(
        main_git.is_dir(),
        "resolved common dir must be a directory, not the gitfile"
    );
    assert!(
        !wt.join(".git").is_dir(),
        "linked worktree .git is a gitfile, not a directory"
    );
    cleanup(&main, &wt);
}

/// A non-git dir falls back to its own canonical dir name, so memory still
/// works outside a repo (just not shared across worktrees).
#[test]
fn test_slug_non_git_dir() {
    let dir = temp_dir().join(format!("houyi-slug-nogit-{}", id()));
    drop(remove_dir_all(&dir));
    create_dir_all(&dir).expect("mkdir non-git");
    assert_eq!(
        git_canonical_slug(&dir),
        format!("houyi-slug-nogit-{}", id())
    );
    drop(remove_dir_all(&dir));
}

/// The enter/exit tools exist only when there is something to isolate: a
/// workspace AND a sandbox session. With either missing nothing registers,
/// so the model is never offered a switch it cannot perform.
#[test]
fn test_controller_registers_tools() {
    let dir = temp_dir().join(format!("houyi-wire-wt-{}", id()));
    drop(remove_dir_all(&dir));
    create_dir_all(&dir).expect("mkdir");
    let sandbox: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&dir).expect("sandbox"));
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());

    let mut tools = ToolRegistry::new();
    let wired = wire_worktree_controller(
        Some(dir.as_path()),
        Some(&sandbox),
        &store,
        SessionId::new(),
        &mut tools,
        &gate,
    );
    assert!(wired.is_some(), "controller built when both resolved");
    assert!(
        tools.get("enter_worktree").is_some(),
        "enter_worktree registered"
    );
    assert!(
        tools.get("exit_worktree").is_some(),
        "exit_worktree registered"
    );

    let mut bare = ToolRegistry::new();
    let unwired = wire_worktree_controller(None, None, &store, SessionId::new(), &mut bare, &gate);
    assert!(unwired.is_none(), "None when no workspace or sandbox");
    assert!(
        bare.get("enter_worktree").is_none(),
        "no tools registered when unwired"
    );
    remove_dir_all(&dir).ok();
}
