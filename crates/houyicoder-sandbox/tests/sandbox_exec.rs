//! Real sandbox-exec integration tests: heredoc survives the single-quote
//! wrap, TMPDIR inside the fence is the per-session dir, mktemp works
//! against that dir, and bash constructs run under the session shell. These
//! spawn real sandbox-exec, so they live in the integration suite rather
//! than the unit gate. A run that is itself inside a fence cannot apply a
//! nested one and skips cleanly instead of reporting a false regression.

#![cfg(target_os = "macos")]
#![allow(clippy::disallowed_methods)]

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_context::ExecConfig;
use houyicoder_resilience::BreakerState;
use houyicoder_resilience::resource_breaker::{ResourceBreaker, ResourceBreakerConfig};
use houyicoder_sandbox::MacSeatbeltSession;
use std::fs::create_dir;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

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

#[tokio::test]
async fn test_ps_runs_under_fence() {
    let s = MacSeatbeltSession::new().unwrap();
    if nested_fence(&s).await {
        return;
    }
    let out = s
        .exec_with_config("ps -ax -o pid", ExecConfig::default())
        .await
        .expect("ps should run");
    assert!(out.is_success(), "ps should succeed: {out:?}");
    assert!(
        out.stdout.lines().count() >= 2,
        "ps should list at least the header and one process: {out:?}"
    );
}

#[tokio::test]
async fn test_tmpdir_env_session_dir() {
    let s = MacSeatbeltSession::new().unwrap();
    if nested_fence(&s).await {
        return;
    }
    let want = s.tmpdir().to_string_lossy().into_owned();
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

#[tokio::test]
async fn test_glob_for_basename_nonempty() {
    let s = MacSeatbeltSession::new().unwrap();
    if nested_fence(&s).await {
        return;
    }
    create_dir(s.workspace().join("adir")).expect("mkdir subdir");
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

#[tokio::test]
async fn test_clean_command_closed() {
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
