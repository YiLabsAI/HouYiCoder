//! Pure-logic sandbox tests that do not spawn sandbox-exec. The real-spawn
//! integration tests moved to tests/sandbox_exec.rs so the unit gate stays
//! free of OS-subsystem calls.

#![allow(clippy::disallowed_methods)]

use super::super::*;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_context::ExecConfig;

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

#[test]
fn test_set_unfenced_state() {
    crate::set_unfenced(true);
    assert!(crate::is_unfenced());
    crate::set_unfenced(false);
}

#[test]
fn test_workspace_and_tmpdir_accessors() {
    let s = MacSeatbeltSession::new().expect("session");
    assert!(s.workspace().exists(), "workspace dir exists");
    assert!(s.tmpdir().exists(), "tmpdir exists");
}
