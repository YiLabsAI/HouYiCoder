//! Landlock smoke binary. Constructs a Linux sandbox session, which probes
//! the fence helper, then drives real commands through exec and asserts the
//! kernel denies paths outside the grant set while the workspace and a
//! runtime-granted directory stay writable, and that revoking the grant
//! takes effect on the next spawn. Built only when the enforce feature is
//! on; the source is cfg-gated to linux. Prints PASS per assertion, SKIP
//! when the kernel reports no enforcement, and exits non-zero on failure.

#[cfg(target_os = "linux")]
fn fail(message: &str) -> ! {
    eprintln!("FAIL: {message}");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    use houyicoder_api::sandbox::{FenceStatus, SandboxSession};
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    // With the hatch set the helper never fences, so every probe below would
    // look unfenced and the smoke would report SKIP -- indistinguishable from
    // a kernel with no Landlock. This binary is the only check of the real
    // fence, so refuse.
    if std::env::var("HOUYICODER_SANDBOX_NO_ENFORCE").is_ok_and(|v| v == "1") {
        eprintln!(
            "FAIL: HOUYICODER_SANDBOX_NO_ENFORCE=1 disables the fence this binary verifies; unset it and re-run"
        );
        process::exit(2);
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();

    // Probes outside the future workspace, created before any fence exists.
    // They are siblings of the workspace under /tmp, which the ruleset never
    // allows, so the spawned shell must be denied on all of them.
    let denied_read = PathBuf::from(format!("/tmp/landlock-smoke-denied-{pid}-{nanos}"));
    let denied_write = PathBuf::from(format!("/tmp/landlock-smoke-write-{pid}-{nanos}"));
    let granted = PathBuf::from(format!("/tmp/landlock-smoke-grant-{pid}-{nanos}"));
    fs::write(&denied_read, b"secret").expect("write denied-read probe");
    fs::create_dir_all(&granted).expect("create granted dir");

    let cleanup = || {
        let _result = fs::remove_file(&denied_read);
        let _result = fs::remove_file(&denied_write);
        let _result = fs::remove_dir_all(&granted);
    };

    let session =
        houyicoder_sandbox::LinuxLandlockSession::new().expect("construct landlock session");
    let status = session.fence_status();
    if !matches!(status, FenceStatus::Enforced) {
        eprintln!(
            "SKIP: fence not enforced ({status:?}): kernel lacks landlock or the helper is missing"
        );
        cleanup();
        return;
    }

    // Read of the sibling probe through the fenced shell: must be denied.
    let out = session
        .exec(&format!("cat {}", denied_read.display()))
        .await
        .unwrap_or_else(|e| fail(&format!("exec cat: {e}")));
    if out.exit_code == Some(0) {
        cleanup();
        fail("read of denied path succeeded; landlock did not fence the spawned tree");
    }
    eprintln!("PASS: denied read blocked (exit {:?})", out.exit_code);

    // Write to a sibling path: must also be denied.
    let out = session
        .exec(&format!("touch {}", denied_write.display()))
        .await
        .unwrap_or_else(|e| fail(&format!("exec touch: {e}")));
    if out.exit_code == Some(0) {
        cleanup();
        fail("write to denied path succeeded; landlock did not fence the write");
    }
    eprintln!("PASS: denied write blocked (exit {:?})", out.exit_code);

    // Control: the workspace stays writable under the fence.
    let allowed = session.workspace_root().join("allowed.txt");
    let out = session
        .exec(&format!("touch {}", allowed.display()))
        .await
        .unwrap_or_else(|e| fail(&format!("exec workspace touch: {e}")));
    if out.exit_code != Some(0) || !allowed.exists() {
        cleanup();
        fail("workspace-internal write was denied");
    }
    eprintln!("PASS: workspace-internal write allowed");

    // A runtime grant reaches the next spawn: the helper receives the live
    // grant set in argv, so a directory added after construction is writable
    // immediately and, once revoked, denied again on the spawn after that.
    let granted_str = granted.to_str().expect("granted dir is utf-8");
    session
        .add_working_dir(granted_str)
        .unwrap_or_else(|e| fail(&format!("add_working_dir: {e}")));
    let after_grant = granted.join("after-grant.txt");
    let out = session
        .exec(&format!("touch {}", after_grant.display()))
        .await
        .unwrap_or_else(|e| fail(&format!("exec granted touch: {e}")));
    if out.exit_code != Some(0) {
        cleanup();
        fail("write into the runtime-granted directory was denied");
    }
    eprintln!("PASS: runtime grant writable on the next spawn");

    session.remove_working_dir(granted_str);
    let after_revoke = granted.join("after-revoke.txt");
    let out = session
        .exec(&format!("touch {}", after_revoke.display()))
        .await
        .unwrap_or_else(|e| fail(&format!("exec revoked touch: {e}")));
    if out.exit_code == Some(0) {
        cleanup();
        fail("write into the revoked directory succeeded; the spawn reused a stale grant set");
    }
    eprintln!("PASS: revoked grant denied on the next spawn");

    cleanup();
    eprintln!("landlock_smoke: all assertions passed");
    // session drops here, removing the workspace temp dir.
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("landlock_smoke: skipped (not linux)");
}
