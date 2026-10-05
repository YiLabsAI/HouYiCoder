//! Cross-platform consistency suite for the sandbox backends. One set of
//! behavioral assertions runs against whichever PlatformSession the host
//! provides, so a grant, a revocation and an escape attempt mean the same
//! thing on every platform: the resolver and exec groups run everywhere, and
//! the kernel group runs whenever the host's path fence is actually live
//! (macOS always; Linux when the helper and kernel agree, never under the
//! test-suite enforcement hatch -- the landlock smoke example exercises that
//! path on demand; Windows never, its job object carries no path primitive).

#![cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]

use houyicoder_api::sandbox::{FenceStatus, SandboxSession};
use houyicoder_context::{ExecConfig, SandboxError};
use houyicoder_sandbox::PlatformSession;
use std::path::{Path, PathBuf};

fn session() -> PlatformSession {
    PlatformSession::new().expect("platform session")
}

/// A directory outside the session workspace, created by the test process
/// (which is never fenced) so the child's view is the only thing under test.
fn external_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "houyicoder-consistency-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create external dir");
    dunce::canonicalize(&dir).expect("canonical external dir")
}

fn write_cmd(path: &str) -> String {
    if cfg!(windows) {
        format!("echo x> {path}")
    } else {
        format!("touch {path}")
    }
}

/// One command writing to both streams, in the host shell's syntax: cmd
/// separates with an ampersand and redirects through 1>&2, a unix shell with
/// a semicolon and >&2.
fn streams_cmd() -> String {
    if cfg!(windows) {
        "echo out & echo err 1>&2".to_string()
    } else {
        "echo out; echo err >&2".to_string()
    }
}

/// A command that outlives the wall timeout under test. It never runs to
/// completion: the timeout group-kills it, so the test costs the timeout
/// budget, not the command's natural duration.
fn outlive_cmd(wall_ms: u64) -> String {
    if cfg!(windows) {
        let secs = wall_ms / 1000 + 5;
        format!("ping -n {secs} 127.0.0.1 > nul")
    } else {
        let secs = wall_ms / 1000 + 5;
        format!("sleep {secs}")
    }
}

/// True when the host's kernel path fence is live for this session, so
/// spawn-level allow and deny assertions are meaningful.
fn kernel_path_fence_live(s: &PlatformSession) -> bool {
    !cfg!(windows) && matches!(s.fence_status(), FenceStatus::Enforced)
}

// ---------------------------------------------------------------------------
// Resolver group: the application-level boundary every backend must provide,
// independent of whether the kernel fence engaged.
// ---------------------------------------------------------------------------

#[test]
fn test_resolver_rejects_external_paths() {
    let s = session();
    let outside = external_dir("reject");
    let target = outside.join("probe.txt");
    std::fs::write(&target, b"x").expect("write probe");
    let target_str = target.to_str().expect("target str");

    assert!(matches!(
        s.resolve(target_str),
        Err(houyicoder_context::SandboxError::PathTraversal(_))
    ));
    assert!(matches!(
        s.resolve_write(target_str),
        Err(houyicoder_context::SandboxError::PathTraversal(_))
    ));
    std::fs::remove_dir_all(&outside).ok();
}

#[test]
fn test_read_grant_blocks_writes() {
    let s = session();
    let outside = external_dir("read-grant");
    let target = outside.join("notes.txt");
    std::fs::write(&target, b"x").expect("write target");
    let dir_str = outside.to_str().expect("dir str");
    let target_str = target.to_str().expect("target str");

    s.add_reading_dir(dir_str).expect("read grant");
    let resolved = s.resolve(target_str).expect("read admitted");
    assert!(resolved.starts_with(&outside));
    assert!(matches!(
        s.resolve_write(target_str),
        Err(houyicoder_context::SandboxError::PathTraversal(_))
    ));

    s.remove_working_dir(dir_str);
    assert!(matches!(
        s.resolve(target_str),
        Err(houyicoder_context::SandboxError::PathTraversal(_))
    ));
    std::fs::remove_dir_all(&outside).ok();
}

#[test]
fn test_write_grant_admits_writes() {
    let s = session();
    let outside = external_dir("write-grant");
    let target = outside.join("notes.txt");
    std::fs::write(&target, b"x").expect("write target");
    let dir_str = outside.to_str().expect("dir str");
    let target_str = target.to_str().expect("target str");

    s.add_working_dir(dir_str).expect("write grant");
    let resolved = s.resolve_write(target_str).expect("write admitted");
    assert!(resolved.starts_with(&outside));
    let resolved_read = s.resolve(target_str).expect("read admitted");
    assert!(resolved_read.starts_with(&outside));

    s.remove_working_dir(dir_str);
    assert!(matches!(
        s.resolve_write(target_str),
        Err(houyicoder_context::SandboxError::PathTraversal(_))
    ));
    std::fs::remove_dir_all(&outside).ok();
}

#[test]
fn test_grant_round_trip() {
    let s = session();
    let write_dir = external_dir("round-trip-w");
    let read_dir = external_dir("round-trip-r");
    let write_str = write_dir.to_str().expect("write str");
    let read_str = read_dir.to_str().expect("read str");

    s.add_working_dir(write_str).expect("write grant");
    s.add_reading_dir(read_str).expect("read grant");

    let writing = s.writing_dirs();
    assert!(
        writing.iter().any(|d| Path::new(d) == write_dir),
        "the write grant must round-trip: {writing:?}"
    );
    let reading = s.reading_dirs();
    assert!(
        reading.iter().any(|d| Path::new(d) == read_dir),
        "the read grant must round-trip: {reading:?}"
    );
    assert!(
        !writing.iter().any(|d| Path::new(d) == read_dir),
        "a read grant must not appear as writable: {writing:?}"
    );

    s.remove_working_dir(write_str);
    s.remove_working_dir(read_str);
    assert!(
        !s.working_dirs().iter().any(|d| Path::new(d) == write_dir),
        "revocation must clear the listing"
    );
    std::fs::remove_dir_all(&write_dir).ok();
    std::fs::remove_dir_all(&read_dir).ok();
}

#[test]
fn test_duplicate_grant_single_entry() {
    let s = session();
    let outside = external_dir("duplicate");
    let dir_str = outside.to_str().expect("dir str");

    s.add_working_dir(dir_str).expect("first grant");
    s.add_working_dir(dir_str).expect("second grant");
    let count = s
        .writing_dirs()
        .iter()
        .filter(|d| Path::new(d.as_str()) == outside)
        .count();
    assert_eq!(count, 1, "a repeated grant must stay one entry");
    std::fs::remove_dir_all(&outside).ok();
}

// ---------------------------------------------------------------------------
// Exec group: the spawn path every backend runs whether or not the kernel
// fence engaged. Ungated on purpose: stream plumbing, exit-code passthrough
// and the wall timeout are backend-shared behavior that even a resolver-only
// boundary owes the caller.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_exec_round_trip() {
    let s = session();
    let r = s.exec(&streams_cmd()).await.expect("exec streams");
    assert!(r.is_success(), "stderr: {}", r.stderr);
    assert_eq!(r.stdout.trim(), "out", "stdout must pass through");
    assert_eq!(r.stderr.trim(), "err", "stderr must pass through");

    let failed = s.exec("exit 3").await.expect("exec exit code");
    assert_eq!(
        failed.exit_code,
        Some(3),
        "a non-zero exit is a result, not an error"
    );
}

#[tokio::test]
async fn test_exec_wall_timeout() {
    let s = session();
    let config = ExecConfig {
        wall_timeout_ms: 100,
        ..ExecConfig::default()
    };
    let err = s
        .exec_with_config(&outlive_cmd(config.wall_timeout_ms), config)
        .await
        .expect_err("the wall timeout must fire");
    assert!(matches!(err, SandboxError::Timeout(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Kernel group: spawn-level assertions, live only where the path fence is.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_kernel_denies_external_write() {
    let s = session();
    if !kernel_path_fence_live(&s) {
        return;
    }
    let outside = external_dir("kernel-deny");
    let target = outside.join("escape.txt");

    let _denied = s
        .exec(&write_cmd(target.to_str().expect("target str")))
        .await;
    assert!(
        !target.exists(),
        "the kernel fence must refuse a write outside the grant set"
    );

    // Positive control: the workspace itself stays writable, so the denial
    // above is path-scoped and not a blanket write failure.
    let inside = s.workspace_root().join("inside.txt");
    let r = s
        .exec(&write_cmd(inside.to_str().expect("inside str")))
        .await
        .expect("workspace exec");
    assert!(r.is_success(), "stderr: {}", r.stderr);
    assert!(inside.exists(), "the workspace write must succeed");
    std::fs::remove_dir_all(&outside).ok();
}

#[tokio::test]
async fn test_kernel_honors_runtime_grant() {
    let s = session();
    if !kernel_path_fence_live(&s) {
        return;
    }
    let outside = external_dir("kernel-grant");
    let dir_str = outside.to_str().expect("dir str");

    s.add_working_dir(dir_str).expect("grant");
    let granted_file = outside.join("granted.txt");
    let r = s
        .exec(&write_cmd(granted_file.to_str().expect("granted str")))
        .await
        .expect("granted exec");
    assert!(
        granted_file.exists(),
        "a runtime grant must reach the fence (stderr: {})",
        r.stderr
    );

    s.remove_working_dir(dir_str);
    let revoked_file = outside.join("revoked.txt");
    let _denied = s
        .exec(&write_cmd(revoked_file.to_str().expect("revoked str")))
        .await;
    assert!(
        !revoked_file.exists(),
        "revoking a grant must deny it at the next spawn"
    );
    std::fs::remove_dir_all(&outside).ok();
}

#[tokio::test]
async fn test_kernel_allows_multiprocess() {
    let s = session();
    if !kernel_path_fence_live(&s) {
        return;
    }
    // A pipeline forces the shell to fork: per-user process rlimits armed at
    // spawn would fail it with EAGAIN on a busy desktop, so this pins the
    // resource-fence semantics (wall timeout plus group kill, no per-spawn
    // rlimits) through a real fenced exec.
    let inside = s.workspace_root().join("pipe.txt");
    let r = s
        .exec(&format!("echo hi | tee {}", inside.display()))
        .await
        .expect("pipeline exec");
    assert!(
        r.is_success(),
        "a pipeline must fork under the fence; stderr: {}",
        r.stderr
    );
    assert_eq!(
        std::fs::read_to_string(&inside).expect("pipe file").trim(),
        "hi"
    );
    std::fs::remove_file(&inside).ok();
}

#[tokio::test]
async fn test_kernel_scratch_temp() {
    let s = session();
    if !kernel_path_fence_live(&s) {
        return;
    }
    // The fence grants nothing under the shared host temp root, so a write
    // through TMPDIR only succeeds when the session redirected it into the
    // granted scratch dir. The probe goes through the shell variable rather
    // than a tool: macOS mktemp ignores TMPDIR in favor of the per-user
    // Darwin temp, while the scratch contract under test is the redirect.
    let r = s
        .exec("echo x > \"$TMPDIR/scratch-probe\" && cat \"$TMPDIR/scratch-probe\"")
        .await
        .expect("scratch exec");
    assert!(
        r.is_success(),
        "scratch temp must land inside the fence; stderr: {}",
        r.stderr
    );
    assert_eq!(r.stdout.trim(), "x", "the scratch write must read back");
}
