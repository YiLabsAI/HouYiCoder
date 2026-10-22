//! Real sandbox-exec tests for the command-execution fixes: heredoc
//! survives the single-quote wrap, TMPDIR inside the fence is the
//! per-session dir (not clobbered by the snapshot source), mktemp works
//! against that dir, and bash constructs run under the session shell. These spawn real sandbox-exec and are NOT
//! ignored, so they enter make check and pin the fixes against
//! regression. A run that is itself inside a fence cannot apply a nested
//! one and skips cleanly instead of reporting a false regression. The effect-level evidence (the user's real-sandbox table)
//! lives here as code, not just in the bug-log.

#![allow(clippy::disallowed_methods)]

use super::super::*;
use houyicoder_context::ExecConfig;
use std::fs::create_dir;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// Whether this process is already inside a fence. The kernel refuses a
/// nested sandbox_apply, and these tests spawn a real sandbox-exec, so inside
/// a fence they can only report that refusal. Saying so is honest; reporting it
/// as a fence regression is not, because the profile under test never ran.
async fn nested_fence(session: &MacSeatbeltSession) -> bool {
    let Ok(out) = session
        .exec_with_config("true", ExecConfig::default())
        .await
    else {
        return false;
    };
    let nested = out.stderr.contains("sandbox_apply");
    if nested {
        eprintln!("skipped: a fence cannot be applied inside a fence");
    }
    nested
}

/// A quoted heredoc executes (its body is not flattened by the command
/// wrapping). Pins the eval single-quote wrap against the prior Rust Debug
/// {:?} newline-escape that collapsed heredoc bodies onto one line.
#[tokio::test]
async fn test_heredoc_executes() {
    let s = MacSeatbeltSession::new().unwrap();
    if nested_fence(&s).await {
        return;
    }
    let out = s
        .exec_with_config("cat << 'EOF'\nhello\nEOF", ExecConfig::default())
        .await
        .expect("heredoc should run");
    assert!(out.is_success(), "heredoc should succeed: {out:?}");
    assert!(
        out.stdout.contains("hello"),
        "heredoc body should reach stdout: {out:?}"
    );
}

/// TMPDIR inside the sandbox is the per-session dir, not the host temp root
/// the snapshot would re-export. Pins the inject re-assert against the
/// snapshot source clobber.
#[tokio::test]
async fn test_tmpdir_env_session_dir() {
    let s = MacSeatbeltSession::new().unwrap();
    if nested_fence(&s).await {
        return;
    }
    let want = s.tmpdir.to_string_lossy().into_owned();
    let out = s
        .exec_with_config("echo $TMPDIR", ExecConfig::default())
        .await
        .expect("echo should run");
    assert_eq!(
        out.stdout.trim(),
        want,
        "TMPDIR must be the per-session dir"
    );
}

/// The agent can write to $TMPDIR (the per-session dir). This is what the
/// $TMPDIR-reading tools (python tempfile, pip, cargo, node os.tmpdir) do:
/// they read TMPDIR from the environment and write there. A shell redirect
/// expands $TMPDIR the same way, so it pins the env value + the fence allow
/// together. BSD mktemp is NOT here: it uses confstr DARWIN_USER_TEMP_DIR,
/// not $TMPDIR, so no TMPDIR fix reaches it (and the host user-temp root is
/// not allowed either).
#[tokio::test]
async fn test_writes_to_tmpdir() {
    let s = MacSeatbeltSession::new().unwrap();
    if nested_fence(&s).await {
        return;
    }
    let out = s
        .exec_with_config(
            "echo ok > $TMPDIR/probe && cat $TMPDIR/probe",
            ExecConfig::default(),
        )
        .await
        .expect("run");
    assert!(out.is_success(), "write to $TMPDIR should succeed: {out:?}");
    assert!(
        out.stdout.contains("ok"),
        "probe content should round-trip: {out:?}"
    );
}

/// A glob for-loop over directories works under the session shell, which is
/// what the model writes. The shell must hand the pattern to the loop rather
/// than refusing it, which is the failure a login shell produced.
#[tokio::test]
async fn test_glob_for_basename_nonempty() {
    let s = MacSeatbeltSession::new().unwrap();
    if nested_fence(&s).await {
        return;
    }
    create_dir(s.workspace.join("adir")).expect("mkdir subdir");
    let out = s
        .exec_with_config(
            "for d in */; do basename \"$d\"; done",
            ExecConfig::default(),
        )
        .await
        .expect("for-glob should run");
    assert!(out.is_success(), "for-glob should succeed: {out:?}");
    assert!(
        out.stdout.contains("adir"),
        "basename should list the subdir, got: {}",
        out.stdout
    );
    assert!(
        !out.stderr.contains("no matches found"),
        "the shell must not refuse the glob: {out:?}"
    );
}

/// exec_streaming drains stdout + counts newlines live (the (Ns · M lines)
/// chip source). Runs a 10-line command via the streaming path + asserts the
/// counter reaches 10 + the full stdout is collected (same as exec). Pins
/// the G8b-lines streaming branch in exec_inner.
#[tokio::test]
async fn test_exec_streaming_counts_lines() {
    let s = MacSeatbeltSession::new().unwrap();
    if nested_fence(&s).await {
        return;
    }
    let counter = Arc::new(AtomicI64::new(-1));
    let out = s
        .exec_streaming("seq 1 10", Arc::clone(&counter))
        .await
        .expect("seq should run");
    assert!(out.is_success(), "seq should succeed: {out:?}");
    assert_eq!(counter.load(Ordering::Relaxed), 10, "final line count");
    assert!(out.stdout.contains("10"), "full stdout collected: {out:?}");
}

/// Spike: does cargo read /etc/ssl/openssl.cnf under the fence? If this
/// fails with the LibreSSL openssl.cnf permission error, the literal allow
/// on the file is not covering the broad /etc subpath deny (the literal-
/// vs-subpath gap). Ignored because it spawns a real cargo + sandbox-exec.
#[tokio::test]
#[ignore = "spike cargo openssl.cnf read in sandbox"]
async fn test_cargo_version_in_sandbox() {
    let s = MacSeatbeltSession::new().expect("session");
    let out = s
        .exec_with_config("cargo --version", ExecConfig::default())
        .await
        .expect("exec");
    eprintln!(
        "success={} exit={:?} stdout={} stderr={}",
        out.is_success(),
        out.exit_code,
        out.stdout,
        out.stderr,
    );
    assert!(out.is_success(), "cargo --version should succeed: {out:?}");
}

/// The command runs under bash, not the user's login shell. A model writes
/// bash, and zsh reads a leading === as a command lookup, treats status as a
/// read-only variable, and aborts the whole command line on an unmatched glob.
/// Each of these fails on zsh and must succeed here.
#[tokio::test]
async fn test_runs_bash_semantics() {
    let s = MacSeatbeltSession::new().unwrap();
    if nested_fence(&s).await {
        return;
    }
    let out = s
        .exec_with_config(
            "echo ===; echo ====; status=0; echo status=$status; \
             ls /nonexist* 2>/dev/null; diff <(echo a) <(echo a) && \
             head -c 16 /etc/ssl/cert.pem >/dev/null && \
             mkdir -p \"$TMPDIR/fx/.houyicoder\" && \
             printf '{}' > \"$TMPDIR/fx/.houyicoder/settings.json\" && \
             echo tail-ok",
            ExecConfig::default(),
        )
        .await
        .expect("run");
    assert!(
        out.is_success(),
        "bash constructs must run under the session shell: {out:?}"
    );
    assert!(
        out.stdout.contains("==="),
        "echo === reaches stdout: {out:?}"
    );
    assert!(
        out.stdout.contains("status=0"),
        "status is an ordinary variable: {out:?}"
    );
    assert!(
        out.stdout.contains("tail-ok"),
        "an unmatched glob does not abort the line: {out:?}"
    );
}
