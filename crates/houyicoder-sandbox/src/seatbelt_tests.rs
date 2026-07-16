//! Sandbox tests, split out of lib.rs so that file stays under the size gate.
//! Inlined via the path attribute so the tests keep use-super access to the
//! private helpers (kill process group, tree cpu secs, mkdtemp, TreeKillGuard).
//!
// ! Tests shell out to pgrep to measure process-tree state; those spawns are test ! scaffolding, not production spawns, so the spawn-chokepoint rule is allowed ! here. Production code routes every spawn through the launcher.

#![allow(clippy::disallowed_methods)]

use super::*;
use crate::{ProfileSpec, render};
use houyicoder_resilience::BreakerState;
use houyicoder_resilience::resource_breaker::{ResourceBreakerConfig, TripReason};

#[tokio::test]
async fn test_breaker_open_refuses_spawn() {
    // A pre-tripped breaker (Open) makes exec refuse the spawn up front
    // and surface BreakerOpen, so the agent can self-correct (retry after the cool-down). No sandbox-exec spawn happens — deterministic, no timing. The breaker trips because 2 in-flight procs exceed cap 1.
    let breaker = Arc::new(ResourceBreaker::new(ResourceBreakerConfig {
        in_flight_proc_cap: 1,
        ..ResourceBreakerConfig::default()
    }));
    breaker.record(SpawnEvent::Start { proc_count: 2 });
    assert_eq!(breaker.state(), BreakerState::Open);
    let s = MacSeatbeltSession::new()
        .unwrap()
        .with_breaker(breaker.clone());
    let r = s
        .exec_with_config("echo should-not-run", ExecConfig::default())
        .await;
    assert!(
        matches!(r, Err(SandboxError::BreakerOpen(_))),
        "Open breaker should refuse spawn, got {r:?}"
    );
}

#[tokio::test]
async fn test_clean_command_closed() {
    // A clean fast command under a default breaker records success and
    // never trips: the tree CPU is ~0 (echo exits before ps sees it) and exceeded_budget is false. Confirms the wiring does not false-trip on normal commands.
    let breaker = Arc::new(ResourceBreaker::new(ResourceBreakerConfig::default()));
    let s = MacSeatbeltSession::new()
        .unwrap()
        .with_breaker(breaker.clone());
    let r = s
        .exec_with_config("echo breaker-clean", ExecConfig::default())
        .await;
    assert!(r.is_ok(), "clean command should succeed, got {r:?}");
    assert_eq!(breaker.state(), BreakerState::Closed);
    assert!(breaker.trip_reason().is_none());
}

#[test]
fn test_span_guard_records_end() {
    // The guard pairs Start with End on the normal path: finish() sets the
    // measured outcome, Drop records End. Three exceeded budgets trip the consecutive-fail breaker (per-cmd threshold = 3 by default). pgid 0 makes tree_cpu_secs a no-op (returns 0) so no ps is spawned here.
    let breaker = Arc::new(ResourceBreaker::new(ResourceBreakerConfig::default()));
    for _ in 0..3 {
        breaker.record(SpawnEvent::Start { proc_count: 1 });
        let mut g = BreakerSpanGuard::new(breaker.clone(), 0);
        g.finish(0, true);
        drop(g); // records End{exceeded:true}
    }
    assert_eq!(breaker.state(), BreakerState::Open);
    assert!(matches!(
        breaker.trip_reason(),
        Some(TripReason::PerCmdBudgetExceeded { .. })
    ));
}

#[test]
fn test_span_guard_end() {
    // The cancel path: finish() is NOT called, so Drop measures CPU itself
    // (pgid 0 -> tree_cpu_secs 0) and records End with the default exceeded_budget=false -> record_success resets consecutive fails. Start/End stay balanced (no in-flight leak) even without finish().
    let breaker = Arc::new(ResourceBreaker::new(ResourceBreakerConfig {
        per_cmd_fail_threshold: 2,
        ..ResourceBreakerConfig::default()
    }));
    // One exceeded finish to arm 1 consecutive fail (below threshold).
    breaker.record(SpawnEvent::Start { proc_count: 1 });
    {
        let mut g = BreakerSpanGuard::new(breaker.clone(), 0);
        g.finish(0, true);
    }
    assert_eq!(breaker.state(), BreakerState::Closed);
    // A cancel-style drop (no finish) records End{exceeded:false} -> success.
    breaker.record(SpawnEvent::Start { proc_count: 1 });
    {
        let _g = BreakerSpanGuard::new(breaker.clone(), 0);
        // no finish -> drop records End{exceeded:false, cpu:0}
    }
    assert_eq!(breaker.state(), BreakerState::Closed);
}

// Live: N consecutive wall-timeouts trip the breaker, then the next spawn is refused (BreakerOpen) for the cool-down — the full wiring path through exec_with_config. Ignored (needs sandbox-exec + real seconds).
#[tokio::test]
#[ignore]
async fn test_breaker_opens_consecutive_timeouts() {
    let breaker = Arc::new(ResourceBreaker::new(ResourceBreakerConfig::default()));
    let s = MacSeatbeltSession::new()
        .unwrap()
        .with_breaker(breaker.clone());
    let cfg = ExecConfig {
        wall_timeout_ms: 200,
        ..ExecConfig::default()
    };
    for _ in 0..3 {
        let _outcome = s.exec_with_config("sleep 100", cfg).await;
    }
    assert_eq!(breaker.state(), BreakerState::Open);
    // The next spawn is refused while the breaker is in cool-down.
    let r = s
        .exec_with_config("echo refused", ExecConfig::default())
        .await;
    assert!(
        matches!(r, Err(SandboxError::BreakerOpen(_))),
        "post-trip spawn should be refused, got {r:?}"
    );
}

#[test]
fn test_render_profile_binds_workspace() {
    let p = render(&ProfileSpec::new(
        Path::new("/tmp/ws-123"),
        "/tmp",
        "/Users/test",
        "tag-x",
    ));
    assert!(p.contains("\"/tmp/ws-123\""));
    assert!(p.contains("(deny default (with message \"tag-x\"))"));
    assert!(p.contains("(deny network* (with message \"tag-x\"))"));
    assert!(p.contains("(allow process-exec)"));
    assert!(p.contains("(allow mach-lookup"));
    assert!(p.contains("com.apple.system.logger"));
    assert!(p.contains("(allow ipc-posix-shm)"));
    assert!(p.contains("(allow file-ioctl"));
    assert!(p.contains("(allow file-write*"));
}

#[test]
fn test_resolve_rejects_outside() {
    let s = MacSeatbeltSession::new().unwrap();
    assert!(matches!(
        s.resolve("/etc/passwd").err().unwrap(),
        SandboxError::PathTraversal(_)
    ));
    assert!(matches!(
        s.resolve("../escape").err().unwrap(),
        SandboxError::PathTraversal(_)
    ));
    assert!(s.resolve("ok.txt").is_ok());
}

#[test]
fn test_sandbox_session_is_object() {
    let _boxed: Box<dyn SandboxSession> = Box::new(MacSeatbeltSession::new().unwrap());
}

#[test]
fn test_sandbox_error_variants_display() {
    let cases = [
        (SandboxError::Io("x".into()), "io", "sandbox io error: x"),
        (
            SandboxError::Unsupported("x".into()),
            "unsupported",
            "sandbox unsupported: x",
        ),
        (
            SandboxError::Timeout("x".into()),
            "timeout",
            "sandbox timeout: x",
        ),
        (
            SandboxError::ResourceLimitExceeded("x".into()),
            "resource_limit_exceeded",
            "sandbox resource limit exceeded: x",
        ),
        (
            SandboxError::NotFound("x".into()),
            "not_found",
            "sandbox not found: x",
        ),
        (
            SandboxError::PathTraversal("x".into()),
            "path_traversal",
            "sandbox path traversal: x",
        ),
        (
            SandboxError::InvalidConfig("x".into()),
            "invalid_config",
            "sandbox invalid config: x",
        ),
        (
            SandboxError::SandboxUnavailable("x".into()),
            "sandbox_unavailable",
            "sandbox unavailable: x",
        ),
        (
            SandboxError::BreakerOpen("x".into()),
            "breaker_open",
            "sandbox breaker open (cool-down): x",
        ),
    ];
    for (err, kind, msg) in cases {
        assert_eq!(err.kind(), kind);
        assert_eq!(format!("{err}"), msg);
    }
}

// ExecConfig defaults are industrial-grade (30s CPU, 2GB AS, 256 nproc, 120s wall). Kernel budgets land via cgroup on Linux ); macOS uses wall-timeout + killpg as the fence.
#[test]
fn test_exec_config_default_industrial() {
    let c = ExecConfig::default();
    assert_eq!(c.cpu_secs, 30);
    assert_eq!(c.as_bytes, 2 * 1024 * 1024 * 1024);
    assert_eq!(c.nproc, 256);
    assert_eq!(c.wall_timeout_ms, 120000);
}

// killpg must be a no-op on pgid <= 0 (never kill init or a stale id).
#[test]
fn test_killpg_noop_on_zero() {
    kill_process_group(0);
    kill_process_group(-1);
}

// Live: a fast command captures stdout + exits 0. Ignored — run via
// make integration (needs sandbox-exec on macOS).
#[tokio::test]
#[ignore]
async fn test_exec_captures_stdout() {
    let s = MacSeatbeltSession::new().unwrap();
    let r = s
        .exec_with_config("echo hello-fence", ExecConfig::default())
        .await;
    let r = r.expect("exec ok");
    assert!(r.is_success());
    assert_eq!(r.stdout.trim(), "hello-fence");
}

// Live: wall-clock timeout kills the tree + returns Timeout. The
// orphan bug class: without killpg a sleep grandchild would survive.
#[tokio::test]
#[ignore]
async fn test_long_command_trips_timeout() {
    let s = MacSeatbeltSession::new().unwrap();
    let cfg = ExecConfig {
        wall_timeout_ms: 200,
        ..ExecConfig::default()
    };
    let r = s.exec_with_config("sleep 100", cfg).await;
    assert!(
        matches!(r, Err(SandboxError::Timeout(_))),
        "wall-clock 1s must trip Timeout, got {r:?}"
    );
}

// Live: CPU spin is caught by wall-timeout (macOS has no safe in-child
// setrlimit — pre_exec is unsafe-blocked by workspace deny; Linux cgroup gives the per-cmd CPU budget). Honest name: wall catches it, not SIGXCPU on macOS.
#[tokio::test]
#[ignore]
async fn test_wall_timeout_kills_spin() {
    let s = MacSeatbeltSession::new().unwrap();
    let cfg = ExecConfig {
        wall_timeout_ms: 500,
        ..ExecConfig::default()
    };
    let r = s.exec_with_config("perl -e 'while(1){}'", cfg).await;
    assert!(
        r.is_err(),
        "CPU spin must be killed by wall-timeout, got {r:?}"
    );
}

// Live: the BLOCKER fix — drop/cancel path must killpg the tree, not just
// the direct child. Spawn 2 background sleeps + a long wait, then ABORT
// the exec future (simulating Ctrl-C / cancel) BEFORE the wall-timeout. The TreeKillGuard Drop must killpg the grandchildren. Without the guard this re-introduces the orphan-process problem on the cancel path.
#[tokio::test]
#[ignore]
async fn test_cancel_aborts_process_tree() {
    let s = MacSeatbeltSession::new().unwrap();
    // long wall so the cancel happens BEFORE timeout (tests the drop path,
    // not the timeout path).
    let cfg = ExecConfig {
        wall_timeout_ms: 10000,
        ..ExecConfig::default()
    };
    let cmd = "sleep 100 & sleep 100 & wait";
    let task = tokio::spawn(async move { s.exec_with_config(cmd, cfg).await });
    // Poll for the grandchildren to appear (10ms ticks up to 2s) — event driven, not a fixed sleep. The spawn + fork takes ~10-50ms; a fixed 1s sleep wasted 950ms every run.
    let mut started = false;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let pgrep = std::process::Command::new("pgrep")
            .arg("-lf")
            .arg("sleep 100")
            .output()
            .expect("pgrep");
        if !String::from_utf8_lossy(&pgrep.stdout).is_empty() {
            started = true;
            break;
        }
    }
    assert!(started, "grandchildren never started — spawn failed");
    // cancel the exec future (Ctrl-C analog) — TreeKillGuard must Drop
    // + killpg the whole tree so no grandchildren survive.
    task.abort();
    // Poll for the tree to die rather than a fixed sleep: under parallel
    // test load a fixed window races the killpg reap (the flake that surfaced when the sandbox category ran live tests concurrently). 100ms ticks up to 5s — fast when the tree dies promptly, robust under load.
    let mut leftover = String::new();
    let mut reaped = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let pgrep = std::process::Command::new("pgrep")
            .arg("-lf")
            .arg("sleep 100")
            .output()
            .expect("pgrep");
        leftover = String::from_utf8_lossy(&pgrep.stdout).into_owned();
        if leftover.is_empty() {
            reaped = true;
            break;
        }
    }
    assert!(
        reaped,
        "drop/cancel did not killpg the tree (orphan on cancel path): {leftover}"
    );
}

// Live: the orphan-process fix. 3 background sleep grandchildren inside the fence; wall=1s trips; the grandchildren must NOT survive (killpg reaps the whole tree). Verifies the orphan class is closed.
#[tokio::test]
#[ignore]
async fn test_timeout_kills_orphan_tree() {
    let s = MacSeatbeltSession::new().unwrap();
    let cfg = ExecConfig {
        wall_timeout_ms: 200,
        ..ExecConfig::default()
    };
    let cmd = "sleep 1000 & sleep 1000 & sleep 1000 & wait";
    let r = s.exec_with_config(cmd, cfg).await;
    assert!(matches!(r, Err(SandboxError::Timeout(_))));
    // Poll for the grandchildren to be reaped (10ms ticks up to 2s) — event
    // driven, not a fixed 500ms sleep.
    let mut reaped = false;
    let mut leftover = String::new();
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let pgrep = std::process::Command::new("pgrep")
            .arg("-lf")
            .arg("sleep 1000")
            .output()
            .expect("pgrep");
        leftover = String::from_utf8_lossy(&pgrep.stdout).into_owned();
        if leftover.is_empty() {
            reaped = true;
            break;
        }
    }
    assert!(
        reaped,
        "orphan grandchildren survived (orphan class): {leftover}"
    );
}

#[test]
fn test_working_dir_round_trips() {
    // add_working_dir canonicalizes + dedups + rejects non-directories; the
    // list round-trips through working_dirs; remove drops by canonical path.
    // No sandbox-exec exercised — this is the state-mutation surface the /permissions Workspace verbs drive; the fence re-derivation on exec is covered by the render_profile additional-dirs test.
    use houyicoder_api::sandbox::SandboxSession;
    let s = MacSeatbeltSession::new().expect("session");
    let tmp = std::env::temp_dir().join(format!(
        "hcs-add-dir-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir(&tmp).expect("mkdir");
    // Add is idempotent — the list keeps one entry, not two.
    s.add_working_dir(tmp.to_str().unwrap()).expect("add once");
    s.add_working_dir(tmp.to_str().unwrap())
        .expect("add twice (idempotent)");
    let dirs = s.working_dirs();
    assert_eq!(dirs.len(), 1, "idempotent add: {dirs:?}");
    assert!(
        dirs[0].ends_with(tmp.file_name().unwrap().to_str().unwrap()),
        "canonical path tracked: {dirs:?}"
    );
    // A file (not a directory) is rejected.
    let file = tmp.join("not-a-dir");
    std::fs::write(&file, b"x").expect("write file");
    assert!(
        s.add_working_dir(file.to_str().unwrap()).is_err(),
        "a file path is not a working dir"
    );
    assert_eq!(
        s.working_dirs().len(),
        1,
        "rejected add did not grow the list"
    );
    // Remove drops by canonical path; the list empties.
    s.remove_working_dir(tmp.to_str().unwrap());
    assert!(s.working_dirs().is_empty(), "remove empties the list");
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn test_profile_includes_added_dir() {
    // The exec-time profile rebuild lands a runtime-added dir in the
    // allow-back (read + write). No sandbox-exec exercised — the helper is
    // split out so the re-derivation is unit-testable.
    use houyicoder_api::sandbox::SandboxSession;
    let s = MacSeatbeltSession::new().expect("session");
    // Empty: the pre-rendered profile is reused (no additional subpath).
    let empty = s.current_profile();
    assert!(
        !empty.contains("(subpath \"/tmp/extra-dir-xyz\")"),
        "empty profile has no added dir: {empty}"
    );
    let tmp = std::env::temp_dir().join(format!(
        "hcs-profile-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir(&tmp).expect("mkdir");
    s.add_working_dir(tmp.to_str().unwrap()).expect("add");
    let with_dir = s.current_profile();
    assert!(
        with_dir.contains(tmp.to_str().unwrap()),
        "profile includes the added dir subpath: {with_dir}"
    );
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn test_session_denies_network() {
    let s = MacSeatbeltSession::new().expect("session");
    assert!(s.current_profile().contains("(deny network*"));
    assert!(!s.current_profile().contains("(allow network*)"));
}

#[test]
fn test_posture_reaches_profile() {
    let mut policy = NetworkPolicy::contained();
    policy.egress = houyicoder_api::sandbox::Egress::Unrestricted;
    let s = MacSeatbeltSession::new()
        .expect("session")
        .with_network(policy);
    let p = s.current_profile();
    assert!(p.contains("(allow network*)"), "got {p}");
    assert!(!p.contains("(deny network*"));
}

#[test]
fn test_rerender_keeps_posture() {
    // The drift guard. A session takes two paths to a profile: the one cached at
    // construction, and a fresh render whenever a dir was added or the fence was
    // narrowed. If only the cached path carried the posture, opening the fence
    // would appear to work until the user added a directory, at which point the
    // fence would silently close again mid-session. Both paths must agree.
    use houyicoder_api::sandbox::SandboxSession;
    let mut policy = NetworkPolicy::contained();
    policy.egress = houyicoder_api::sandbox::Egress::Unrestricted;
    let s = MacSeatbeltSession::new()
        .expect("session")
        .with_network(policy);
    let tmp = std::env::temp_dir().join(format!(
        "hcs-net-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir(&tmp).expect("mkdir");
    s.add_working_dir(tmp.to_str().unwrap()).expect("add");
    let p = s.current_profile();
    assert!(
        p.contains(tmp.to_str().unwrap()),
        "the re-render path is the one under test"
    );
    assert!(
        p.contains("(allow network*)"),
        "the re-render must carry the session posture: {p}"
    );
    std::fs::remove_dir_all(&tmp).ok();
}

/// Live fence-extension proof: a dir added via add_working_dir must actually
/// grant the sandboxed bash access to a file in it. Before the add the file is
/// outside the workspace fence (cat denied); after the add the fence's
/// allow-back covers the dir (cat succeeds). This is the "UI add → real-flow
/// enforcement" guarantee for working dirs, exercised through a real
/// sandbox-exec. Ignored (needs macOS sandbox-exec); run with --ignored.
///
/// add_working_dir grants a WRITE root outside the workspace; reads are
/// already open under gap-B, so the control is the write: denied before
/// add, succeeds after.
#[tokio::test]
#[ignore]
async fn test_added_dir_accessible() {
    use houyicoder_api::sandbox::SandboxSession;
    let s = MacSeatbeltSession::new().expect("session");
    // Under HOME (not /tmp, which the fence broadly allows) so the write-deny
    // before add is load-bearing.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let tmp = std::path::PathBuf::from(home).join(format!(
        "houyi-dir-effect-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir(&tmp).expect("mkdir");
    std::fs::write(tmp.join("secret.txt"), b"42").expect("write");
    let path = tmp.join("secret.txt");
    let path_str = path.to_str().unwrap().to_string();

    // Before the add: a write outside the workspace is refused (add_working_dir
    // grants WRITE roots; reads are already open under the gap-B read policy,
    // so the meaningful control is the write).
    let before = s.exec(&format!("echo appended >> {path_str}")).await;
    let before_ok = before.as_ref().map(|r| r.is_success()).unwrap_or(false);
    assert!(
        !before_ok,
        "before add, a write outside the workspace must be denied: {before:?}"
    );

    // Add the dir (takes the DIRECTORY, not the file); the write allow-back
    // extends on the next exec.
    s.add_working_dir(tmp.to_str().unwrap())
        .expect("add working dir");

    // After the add: the write succeeds and the appended content is visible.
    let after_write = s
        .exec(&format!("echo appended >> {path_str}"))
        .await
        .expect("write after add");
    assert!(
        after_write.is_success(),
        "after add, the write to the added dir must succeed: {after_write:?}"
    );
    let after_read = s
        .exec(&format!("cat {path_str}"))
        .await
        .expect("read after add");
    assert!(
        after_read.stdout.contains("appended"),
        "the appended content reaches the file: {}",
        after_read.stdout
    );
    std::fs::remove_dir_all(&tmp).ok();
}

/// Spike (Slice -1, go/no-go gate for the worktree feature): prove the
/// load-bearing bet "narrow fence to a linked worktree + .git allow-back
/// lets git commit still work" holds under a real sandbox-exec. This combo
/// has never been exercised in this repo — the dead new_in_worktree path
/// passed an empty additional list (no .git allow-back) and was never wired,
/// likely because it hit exactly this wall.
///
/// The stress point is the gitfile indirection: a linked worktree's .git is
/// a FILE (gitdir: <repo>/.git/worktrees/<slug>), so git must traverse the
/// indirection to read/write <repo>/.git/worktrees/<slug>/{HEAD,index} and
/// <repo>/.git/{objects,refs}. seatbelt path resolution is realpath-based,
/// and indirection + path allowlist + realpath is the classic blow-up combo.
///
/// Verdict: GO with three design amendments (kept here as a permanent
/// regression of the bet so a profile regression is caught):
/// 1. The .git allow-back path MUST be canonicalized (realpath /private/var,
///    not /var on macOS). add_working_dir already canonicalizes; the
///    narrow_to_worktree port must too.
/// 2. The worktree profile MUST read-allow <repo>/.git/config (git needs to
///    read repo config, else fatal). The mandatory .git/config deny (which
///    blocks credential-helper INSERTION via config writes) is amended to
///    read-allow + write-deny — the fence blocks network so a credential
///    helper cannot exfiltrate even if config points at one.
/// 3. HISTORICAL, now resolved: the worktree session used to set
///    HOME=<worktree> plus switch off the system git config, because two
///    profile denials made git fatal on startup. Both are fixed, so the
///    session no longer touches the environment and this test asserts the
///    unmodified path:
///    (a) the system etc allow-back never reached the kernel, because the
///        broad deny of the etc subpath also covered the etc symlink node and
///        path resolution failed before the leaf rule was evaluated. Fixed by
///        allowing a metadata read of that node;
///    (b) the home git config was denied for read as well as write, and git
///        treats that read failure as fatal. It is now denied for write only,
///        with the read allowed back, so a credential helper still cannot be
///        injected while git works.
///    Rewriting HOME was also lossy in its own right: it hid the user's real
///    git identity and aliases from every command in the session.
///
/// A throwaway temp repo is used so the live repo is never touched. Ignored
/// (needs macOS sandbox-exec); run with --ignored test_narrow_fence_git_works.
#[tokio::test]
#[ignore]
async fn test_narrow_fence_git_works() {
    let parent = std::env::temp_dir().join(format!(
        "houyi-spike-narrow-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&parent).expect("mkdir parent");
    let repo = parent.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
    };
    assert!(run(&["init", "-q"]).unwrap().success(), "git init");
    let _ = run(&["config", "user.email", "spike@x"]).unwrap();
    let _ = run(&["config", "user.name", "spike"]).unwrap();
    assert!(
        run(&["commit", "--allow-empty", "-m", "init", "-q"])
            .unwrap()
            .success(),
        "initial commit so HEAD exists"
    );
    // Linked worktree: <wt>/.git is a gitfile -> the realpath stress point.
    let wt = parent.join("wt");
    assert!(
        run(&["worktree", "add", &wt.to_string_lossy(), "-q"])
            .unwrap()
            .success(),
        "git worktree add"
    );
    // Render the worktree profile: fence=worktree + .git allow-back (canonical
    // to the realpath, amendment 1) + read-allow .git/config (amendment 2).
    let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
    let stag = format!("houyi-spike-{}", std::process::id());
    let repo_git_c = std::fs::canonicalize(repo.join(".git")).expect("canonicalize .git");
    let git_common = repo_git_c.to_string_lossy().into_owned();
    let mut profile =
        render(&ProfileSpec::new(&wt, &tmpdir, &home, &stag).with_additional(&[&git_common]));
    // Amendment 2: read-allow <repo>/.git/config (canonical) + the worktree
    // metadata dir, appended AFTER mandatory_deny so last-match-wins re-allows
    // the read. Writes stay denied (the mandatory file-write* deny still holds).
    profile.push_str(&format!(
        "(allow file-read* (literal \"{}\") (subpath \"{}\"))\n",
        repo_git_c.join("config").to_string_lossy(),
        repo_git_c.join("worktrees").to_string_lossy(),
    ));
    // No environment workaround: git reads its real system and home config
    // here. If either read regresses to a denial, git fatals and this fails,
    // which is the point of asserting the unmodified path.
    let fix = std::process::Command::new("sandbox-exec")
        .arg("-p")
        .arg(&profile)
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(
            "git -c user.email=spike@x -c user.name=spike \
             commit --allow-empty -m spike-fix \
             && git log --oneline -1",
        )
        .current_dir(&wt)
        .output()
        .expect("sandbox-exec");
    assert!(
        fix.status.success(),
        "BET FAILED: narrow fence + .git allow-back + read-allow .git/config must let git commit work: {}",
        String::from_utf8_lossy(&fix.stderr)
    );
    assert!(
        String::from_utf8_lossy(&fix.stdout).contains("spike-fix"),
        "log must show the spike-fix commit: {}",
        String::from_utf8_lossy(&fix.stdout)
    );
    std::fs::remove_dir_all(&parent).ok();
}

/// Narrow state round-trip (no sandbox-exec): build a session fenced to a
/// throwaway repo, git-worktree-add a worktree under it, narrow the fence,
/// assert the effective workspace root moved to the worktree + the guard
/// restore reverts it. Exercises narrow_to_worktree + active_exec_count +
/// the restore closure without spawning sandbox-exec (the narrow is pure
/// state mutation; the profile binds only at exec).
#[tokio::test]
async fn test_narrow_state_round_trip() {
    use houyicoder_api::sandbox::SandboxSession;
    let parent = std::env::temp_dir().join(format!(
        "houyi-narrow-state-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&parent).expect("mkdir parent");
    let repo = parent.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
    };
    assert!(run(&["init", "-q"]).unwrap().success(), "git init");
    let _ = run(&["config", "user.email", "t@x"]).unwrap();
    let _ = run(&["config", "user.name", "t"]).unwrap();
    assert!(
        run(&["commit", "--allow-empty", "-m", "init", "-q"])
            .unwrap()
            .success(),
        "init commit"
    );
    let wt = repo
        .join(".houyicoder")
        .join("worktrees")
        .join("test-narrow");
    std::fs::create_dir_all(wt.parent().unwrap()).expect("mkdir worktree parent");
    assert!(
        run(&["worktree", "add", &wt.to_string_lossy(), "-q"])
            .unwrap()
            .success(),
        "worktree add"
    );
    let s = MacSeatbeltSession::new_in_cwd(&repo).expect("session");
    assert_eq!(s.active_exec_count(), 0, "no in-flight exec at rest");
    let guard = s
        .narrow_to_worktree(&wt, &repo.join(".git"))
        .expect("narrow");
    // Effective workspace root moved to the worktree.
    assert_eq!(
        s.workspace_root().as_ref(),
        wt.canonicalize().unwrap().as_path()
    );
    // The real exec path under the narrow profile: the worktree session binds
    // the fence to the worktree and appends the .git/config read-allow, so a
    // git commit in the worktree succeeds (the bet the spike proved). No
    // environment rewriting is involved any more; git reads its real config.
    // Covers the current_profile narrow branch.
    let commit = s
        .exec("git commit --allow-empty -m narrow-commit")
        .await
        .expect("exec");
    assert!(
        commit.is_success(),
        "narrow fence + .git allow-back lets git commit work: {commit:?}"
    );
    // Restore reverts the fence to the repo root.
    guard.restore().expect("restore");
    assert_eq!(
        s.workspace_root().as_ref(),
        repo.canonicalize().unwrap().as_path()
    );
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
fn test_containment_fenced_blocks_egress() {
    use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
    let s = MacSeatbeltSession::new().unwrap();
    let ws = s.workspace_root();
    match s.coverage() {
        Coverage::Fenced { writable_roots } => {
            assert!(
                writable_roots.iter().any(|r| r.as_path() == ws.as_ref()),
                "fence must cover the session workspace"
            );
        }
        _ => panic!("expected Fenced coverage"),
    }
    assert!(s.would_block(SideEffect::Network).is_some());
    assert!(s.would_block(SideEffect::None).is_none());
}

#[test]
fn test_as_containment_returns_some() {
    let s = MacSeatbeltSession::new().unwrap();
    assert!(s.as_containment().is_some());
}

/// The login-shell snapshot restores PATH inside the sandboxed shell. The
/// sandboxed command runs non-login, so without the snapshot it would see
/// only the minimal inherited PATH. With the snapshot sourced, it sees the
/// user's login PATH (homebrew, etc). Ignored (needs a real shell + login
/// rc); run with --ignored.
#[tokio::test]
#[ignore]
async fn test_login_snapshot_restores_path() {
    use houyicoder_api::sandbox::SandboxSession;
    let s = MacSeatbeltSession::new().expect("session");
    // The login PATH should contain a directory the minimal non-login PATH
    // does not — /usr/bin is in both, but the user's homebrew or /etc/paths.d
    // entries are not. Assert the exec PATH is longer than the bare /bin:/usr
    // floor by checking it contains at least one non-default segment.
    let out = s.exec("echo $PATH").await.expect("exec");
    assert!(out.is_success(), "echo PATH succeeded: {out:?}");
    let path = out.stdout.trim();
    // A login-sourced PATH has more than just /usr/bin:/bin. At minimum it
    // includes /usr/local/bin or /opt/homebrew/bin on a dev mac.
    let has_extra = path
        .split(':')
        .any(|p| p.contains("local/bin") || p.contains("homebrew") || p.contains("/opt/"));
    assert!(
        has_extra,
        "login snapshot restored a PATH beyond the bare floor: {path}"
    );
}
