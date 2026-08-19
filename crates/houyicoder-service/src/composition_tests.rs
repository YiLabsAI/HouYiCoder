//! Composition tests split out for the file-size gate: host handle round-trip,
//! new_for_resume hydration, and the memory self-heal wiring.
use super::memory::heal_memory_index;
use super::worktree::{git_canonical_slug, git_common_dir};
use super::*;
use crate::lifecycle::SessionLeaseStore;
use crate::server::Server;
use std::sync::atomic::AtomicU64;
#[test]
fn test_degrade_passes_success_through() {
    let kept: Option<u8> = degrade_with_notice(Ok::<u8, String>(7), "unused", "unused");
    assert_eq!(
        kept,
        Some(7),
        "a successful attempt must be handed back untouched, so wrapping a \
         construction in the notice does not change what the caller receives"
    );
}

/// A failed attempt becomes an absence rather than a panic or a default, which
/// is what lets the caller decide between substituting something reduced and
/// carrying on without the capability at all. The synthetic error keeps this on
/// the decision itself: reaching it through a real construction failure would
/// mean arranging for a sandbox to be unbuildable, which tests the operating
/// system rather than this branch.
#[test]
fn test_degrade_reports_absence() {
    let lost: Option<u8> = degrade_with_notice(
        Err::<u8, String>("underlying cause".into()),
        "capability could not be built",
        "the feature is off for this run.",
    );
    assert!(
        lost.is_none(),
        "a failed attempt must degrade to None so the caller can substitute or \
         withhold, rather than proceeding with something half-built"
    );
}

/// Build a minimal runner for host-level tests: a stub provider, an empty
/// tool registry, an in-memory store. No sandbox, no real model — the host
/// methods under test never run the agent, they only carry the Arc handle.
fn minimal_runner() -> Runner {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    Runner::with_shared_store(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 1,
            ..RunnerConfig::default()
        },
    )
}

/// Inserting a live runner into the host, then cloning the handle, returns
/// the same Arc<Runner> + the shared seq counter + the gate. set_pushed_count
/// round-trips through the handle so a disconnect flush survives. No live
/// runner is returned for a session the host never held.
#[test]
fn test_host_clones_runner_handle() {
    let session = SessionId::new();
    let runner = Arc::new(minimal_runner());
    let next_seq = Arc::new(AtomicU64::new(0));
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());

    let host = SessionHost::new(SessionLeaseStore::new());
    assert!(
        host.clone_handle(session).is_none(),
        "no handle before insert",
    );
    host.insert(session, runner.clone(), next_seq.clone(), gate.clone());

    let handle = host.clone_handle(session).expect("handle after insert");
    assert!(
        Arc::ptr_eq(&handle.runner, &runner),
        "clone_handle returns the same runner Arc",
    );
    assert!(Arc::ptr_eq(&handle.next_seq, &next_seq));
    assert_eq!(handle.pushed_count, 0, "starts at zero pushed events");

    host.set_pushed_count(session, 7);
    assert_eq!(
        host.clone_handle(session).unwrap().pushed_count,
        7,
        "set_pushed_count round-trips through the handle",
    );
}

/// new_for_resume rebuilds a Server from the host's live handle so the
/// runner + the shared seq counter + the pushed-event cursor survive a
/// prior connection's disconnect. The host reference is retained (host is
/// Some) so the run path can write the parked PendingTurn later.
#[test]
fn test_new_for_resume_hydrates() {
    let session = SessionId::new();
    let runner = Arc::new(minimal_runner());
    let next_seq = Arc::new(AtomicU64::new(0));
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());

    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(session, runner, next_seq, gate);

    let handle = host.clone_handle(session).expect("handle present");
    // The call itself re-hydrates a Server from the host's live handle;
    // the runner Arc + the shared seq counter + the pushed-event cursor
    // flow in from the handle, and the host reference is retained so the
    // run path can write the parked PendingTurn later. (Server's fields
    // are private to the server module; the call covering the body is the
    // assertion here — a behavior-level check lands with the reconnect
    // test that drives serve_session end-to-end.)
    let _server = Server::new_for_resume(
        handle.runner,
        session,
        handle.next_seq,
        handle.pushed_count,
        handle.gate,
        host,
    );
}

fn temp_root() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("composition_test_{seq}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp root");
    dir
}

/// A root with a topic file and no index is stale; heal must rebuild the
/// derived MEMORY.md so recall sees the topic on the first turn.
#[test]
fn test_heal_rebuilds_stale_root() {
    let root = temp_root();
    std::fs::write(
        root.join("topic.md"),
        "---\nname: t\ndescription: d\n---\nbody\n",
    )
    .expect("write topic");
    let provider = houyicoder_memory::MarkdownMemoryProvider::new(root.clone());
    assert!(
        !root.join("MEMORY.md").exists(),
        "precondition: no index yet"
    );
    heal_memory_index(&provider);
    assert!(
        root.join("MEMORY.md").exists(),
        "index rebuilt by self-heal"
    );
    drop(std::fs::remove_dir_all(&root));
}

/// An empty root (no topics) is not stale; heal is a no-op and writes no
/// index. Guards against a regression that eagerly rebuilds nothing into
/// a spurious empty index.
#[test]
fn test_heal_noop_empty_root() {
    let root = temp_root();
    let provider = houyicoder_memory::MarkdownMemoryProvider::new(root.clone());
    heal_memory_index(&provider);
    assert!(
        !root.join("MEMORY.md").exists(),
        "no index written for an empty root"
    );
    drop(std::fs::remove_dir_all(&root));
}

/// A rebuild failure (the root is read-only so the index write fails)
/// must not panic — the error path logs and returns so the store still
/// serves from the topic files. Covers the best-effort error branch.
#[cfg(unix)]
#[test]
fn test_heal_logs_write_failure() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_root();
    std::fs::write(
        root.join("topic.md"),
        "---\nname: t\ndescription: d\n---\nbody\n",
    )
    .expect("write topic");
    // Make the root read-only so the index pointer write fails.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).expect("set read-only");
    let provider = houyicoder_memory::MarkdownMemoryProvider::new(root.clone());
    // Must not panic; the error is logged to stderr (best-effort).
    heal_memory_index(&provider);
    // Restore so cleanup can remove the dir.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).ok();
    drop(std::fs::remove_dir_all(&root));
}

/// True when the git binary is on PATH so the slug tests can run; otherwise
/// they skip (memory still works via the fallback path, just not testable
/// against a real repo here).
fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Create a temp git repo whose dir name is exactly the given name (inside a
/// pid- and seq-unique parent so parallel test runs do not collide on the
/// shared parent dir). One empty commit is made so a linked worktree has a
/// HEAD to branch from.
fn temp_git_repo(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let parent = std::env::temp_dir().join(format!("houyi-slug-{}-{}", std::process::id(), seq));
    drop(std::fs::remove_dir_all(&parent));
    std::fs::create_dir_all(&parent).expect("mkdir parent");
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).expect("mkdir repo");
    std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .arg("init")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git init");
    std::process::Command::new("git")
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
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git commit");
    dir
}

/// A main repo's slug is its own dir name (git-common-dir's parent).
#[test]
fn test_slug_matches_repo_name() {
    if !git_available() {
        eprintln!("skip: git unavailable");
        return;
    }
    let repo = temp_git_repo("mainrepo");
    assert_eq!(git_canonical_slug(&repo), "mainrepo");
    drop(std::fs::remove_dir_all(repo.parent().expect("parent")));
}

/// A linked worktree shares the main repo's slug — the point of the fix:
/// memory lives under one auto-scope dir regardless of which worktree is
/// active. The old slug (workspace dir name) gave each worktree its own dir.
#[test]
fn test_slug_shared_across_worktree() {
    if !git_available() {
        eprintln!("skip: git unavailable");
        return;
    }
    let main = temp_git_repo("sharedmain");
    let wt = main
        .parent()
        .expect("parent")
        .join(format!("sharedwt-{}", std::process::id()));
    let added = std::process::Command::new("git")
        .arg("-C")
        .arg(&main)
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    if !added {
        eprintln!("skip: git worktree add failed");
        drop(std::fs::remove_dir_all(main.parent().expect("parent")));
        return;
    }
    let main_slug = git_canonical_slug(&main);
    let wt_slug = git_canonical_slug(&wt);
    assert_eq!(main_slug, wt_slug, "worktree must share the main repo slug");
    assert_eq!(main_slug, "sharedmain");
    drop(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["worktree", "remove", "--force"])
            .arg(&wt)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
    );
    drop(std::fs::remove_dir_all(main.parent().expect("parent")));
}

/// git_common_dir resolves the .git gitfile indirection for a linked
/// worktree to the main repo's shared .git directory — the path the
/// worktree controller allow-backs into the narrow fence. The raw workspace
/// .git is a gitfile (text pointer) for a linked worktree, so passing that
/// instead would target a non-existent path and git log in the worktree
/// session would fail.
#[test]
fn test_git_dir_resolves_worktree() {
    if !git_available() {
        eprintln!("skip: git unavailable");
        return;
    }
    let main = temp_git_repo("sharedmain2");
    let wt = main
        .parent()
        .expect("parent")
        .join(format!("sharedwt2-{}", std::process::id()));
    let added = std::process::Command::new("git")
        .arg("-C")
        .arg(&main)
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    if !added {
        eprintln!("skip: git worktree add failed");
        drop(std::fs::remove_dir_all(main.parent().expect("parent")));
        return;
    }
    let main_git = std::fs::canonicalize(main.join(".git")).expect("canonicalize main .git");
    let wt_common = git_common_dir(&wt).expect("git_common_dir resolves for linked worktree");
    assert_eq!(
        std::fs::canonicalize(&wt_common).expect("canonicalize wt common"),
        main_git,
        "linked worktree common dir must be the main repo .git, not the gitfile"
    );
    assert!(
        main_git.is_dir(),
        "resolved common dir must be a directory (the main repo .git), not the gitfile"
    );
    // The raw workspace .git is a gitfile (a file), not a directory — the
    // thing git_common_dir exists to avoid passing to the fence.
    assert!(
        !wt.join(".git").is_dir(),
        "linked worktree .git is a gitfile, not a directory"
    );
    drop(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["worktree", "remove", "--force"])
            .arg(&wt)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
    );
    drop(std::fs::remove_dir_all(main.parent().expect("parent")));
}

/// A non-git dir falls back to its own canonical dir name (memory still works,
/// just not shared across worktrees).
#[test]
fn test_slug_non_git_dir() {
    let dir = std::env::temp_dir().join(format!("houyi-slug-nogit-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).expect("mkdir non-git");
    let slug = git_canonical_slug(&dir);
    assert_eq!(slug, format!("houyi-slug-nogit-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
}

/// wire_worktree_controller registers the enter/exit tools when both a
/// workspace + sandbox resolved, returns None (no tools) when either is
/// missing. Covers the composition wiring path.
#[test]
fn test_worktree_controller_registers_tools() {
    use houyicoder_api::sandbox::SandboxSession;
    use houyicoder_core::agent::ToolRegistry;
    use houyicoder_permission::{DefaultModeGate, ModeGate};
    use houyicoder_sandbox::PlatformSession;
    let dir = std::env::temp_dir().join(format!("houyi-wire-wt-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let sandbox: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&dir).expect("sandbox"));
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let mut tools = ToolRegistry::new();
    let c = worktree::wire_worktree_controller(
        Some(dir.as_path()),
        Some(&sandbox),
        &store,
        houyicoder_context::SessionId::new(),
        &mut tools,
        &gate,
    );
    assert!(c.is_some(), "controller built when both resolved");
    assert!(
        tools.get("enter_worktree").is_some(),
        "enter_worktree registered"
    );
    assert!(
        tools.get("exit_worktree").is_some(),
        "exit_worktree registered"
    );
    let mut tools2 = ToolRegistry::new();
    let c2 = worktree::wire_worktree_controller(
        None,
        None,
        &store,
        houyicoder_context::SessionId::new(),
        &mut tools2,
        &gate,
    );
    assert!(c2.is_none(), "None when no workspace/sandbox");
    assert!(tools2.get("enter_worktree").is_none(), "no tools when None");
    std::fs::remove_dir_all(&dir).ok();
}

/// rehydrate_directories bridges the two persistence layers: a directory the
/// user persisted to the rule store (envelope) must reach the in-memory kernel
/// fence (session.add_working_dir) on startup. Effect-level: add a directory
/// to a FileRuleStore, build a fresh sandbox session (empty fence), call
/// rehydrate_directories, assert the fence now contains it. Pins the bridge so
/// a persistent directory auth is not silent on restart.
#[test]
fn test_rehydrate_pours_dirs_fence() {
    use houyicoder_api::sandbox::SandboxSession;
    use houyicoder_permission::{FileRuleStore, RuleStore, Scope};
    use houyicoder_sandbox::PlatformSession;
    let root = std::env::temp_dir().join(format!("houyi-rehydrate-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");
    // A FileRuleStore with temp paths (not default_paths — do not pollute home).
    let store: Arc<dyn RuleStore> = Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ));
    // A target dir the user "persisted" (exists so canonicalize succeeds).
    let target = root.join("authorized-dir");
    std::fs::create_dir_all(&target).expect("mkdir target");
    store
        .add_directory(&target, Scope::Project)
        .expect("add_directory");
    // A sandbox session fenced to a repo (state only — new_in_cwd, no exec).
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    assert!(
        session.working_dirs().is_empty(),
        "fence starts empty before rehydrate"
    );
    super::rehydrate_directories(session.as_ref(), store.as_ref());
    let dirs = session.working_dirs();
    let canonical = std::fs::canonicalize(&target).expect("canonicalize target");
    assert!(
        dirs.iter()
            .any(|d| std::path::Path::new(d.as_str()) == canonical.as_path()),
        "rehydrate must pour the persisted directory into the fence: {dirs:?}"
    );

    // A non-existent persistent dir (stale — deleted since it was persisted)
    // must not crash rehydrate; the error path counts it + the existent dir
    // still re-attaches.
    let stale = root.join("stale-deleted");
    store
        .add_directory(&stale, houyicoder_permission::Scope::Project)
        .expect("add stale dir");
    let session2: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox2"));
    super::rehydrate_directories(session2.as_ref(), store.as_ref());
    let dirs2 = session2.working_dirs();
    assert!(
        dirs2
            .iter()
            .any(|d| std::path::Path::new(d.as_str()) == canonical.as_path()),
        "existent dir still re-attaches when a stale dir fails: {dirs2:?}"
    );
    assert!(
        !dirs2.iter().any(|d| d.contains("stale-deleted")),
        "stale dir does not re-attach: {dirs2:?}"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// ContainmentAdapter delegates boundary_root + boundary_dirs to the wrapped
/// SandboxSession (workspace_root + working_dirs), so the gate's path-bounds
/// pre-check sees the real fence bounds without PlatformSession having to
/// also implement Containment for those. Pins the adapter bridge so a future
/// refactor of the gate's Containment query cannot silently lose the root.
#[test]
fn test_containment_adapter_delegates_boundary() {
    use houyicoder_api::sandbox::{Containment, SandboxSession};
    use houyicoder_sandbox::PlatformSession;
    let root = std::env::temp_dir().join(format!("adapter-bound-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");
    let extra = root.join("extra");
    std::fs::create_dir_all(&extra).expect("mkdir extra");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&root).expect("sandbox"));
    session
        .add_working_dir(&extra.to_string_lossy())
        .expect("add working dir");
    let adapter = super::ContainmentAdapter(session);
    assert_eq!(
        adapter.boundary_root().map(|p| p.to_path_buf()),
        Some(std::fs::canonicalize(&root).unwrap()),
        "boundary_root delegates to session.workspace_root"
    );
    let dirs = adapter.boundary_dirs();
    let cextra = std::fs::canonicalize(&extra).unwrap();
    assert!(
        dirs.iter().any(|d| d == &cextra),
        "boundary_dirs delegates to session.working_dirs: {dirs:?}"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// build_runner wires the fence containment into the gate so the path-bounds
/// validator fires for a grep whose path is outside the workspace. Pins the
/// composition-root ordering: the shared dyn handle must be cloned AFTER
/// with_containment mutates the gate, so Arc::get_mut sees strong_count 1.
/// An earlier clone left the count at 2 and Arc::get_mut silently returned
/// None — the containment wiring was skipped, the path-bounds validator kept
/// a None handle, the gate never asked, and the tool's own confine_path
/// hard-refused instead of surfacing a card. Effect-level: a real temp repo
/// with a workspace manifest, build_runner with that repo as the project,
/// decide on a grep whose path is a sibling outside the repo — must Ask.
#[test]
fn test_build_runner_outside_grep() {
    use houyicoder_permission::{Decision, ModeGate, ToolRequest};
    let root = std::env::temp_dir().join(format!("houyi-wire-{}-{}", std::process::id(), line!()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    std::fs::write(repo.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("manifest");
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    let bundle = super::build_runner(Some(repo.to_string_lossy().into_owned()), None, None);
    let gate = bundle.gate;
    let input = serde_json::json!({"pattern":"x","path":outside.to_string_lossy()});
    // native_requires_approval=false + read-only so the ONLY Ask source is the
    // path-bounds Detection validator: mode_default would otherwise Allow a
    // read-only grep under the default Auto posture, so an Ask here proves the
    // containment wiring reached the gate's pipeline. Asserting the validator
    // name pins that the Ask is path-bounds (not a built-in rule or mode ask),
    // so the test fails if the wiring is skipped.
    let req = ToolRequest {
        tool_name: "grep",
        input: Some(&input),
        is_destructive: false,
        is_read_only: true,
        native_requires_approval: false,
    };
    match gate.decide(&req) {
        Decision::Ask(r) => assert_eq!(
            r.validator, "path-bounds",
            "the outside-grep ask must come from the path-bounds validator (containment wired): {r:?}"
        ),
        other => panic!("outside grep must Ask via path-bounds, got {other:?}"),
    }
    std::fs::remove_dir_all(&root).ok();
}

/// The production composition root wires an LlmSummarizer (real summaries)
/// into the runner, not the default HeuristicSummarizer placeholder. Pins the
/// wiring so a refactor that drops with_summarizer or swaps back to the
/// heuristic fails this test instead of silently regressing compress to a
/// placeholder. Type-level assertion via Summarizer::as_any downcast.
#[test]
fn test_build_runner_wires_summarizer() {
    let root = std::env::temp_dir().join(format!("houyi-sum-{}-{}", std::process::id(), line!()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");
    let bundle = super::build_runner(Some(root.to_string_lossy().into_owned()), None, None);
    assert!(
        bundle.runner.summarizer_is_llm(),
        "production runner must carry an LlmSummarizer, not the heuristic placeholder"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[cfg(test)]
// The resume-path tests live in their own #[path] file so this source
// stays under the file-size gate; the mod body is the included file.
#[path = "composition_resume_tests.rs"]
mod resume_tests;
