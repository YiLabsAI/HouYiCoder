//! Landlock smoke binary. Constructs a LinuxLandlockSession (which applies
//! the Landlock fence to this process at construction), then asserts that a
//! path outside the workspace is denied at the kernel level with EACCES or
//! EPERM. Built only when the enforce feature is on (see Cargo.toml
//! required-features); the source is cfg-gated to linux so non-linux builds
//! compile a no-op main.
//!
//! Run on a Linux host with a Landlock-capable kernel:
//!   cargo run --example landlock_smoke --features enforce
//! Prints PASS when the fence denies as expected, SKIP when the kernel
//! reports no Landlock enforcement (the session degraded to a no-op), and
//! exits non-zero on an unexpected result.

#[cfg(target_os = "linux")]
fn main() {
    use houyicoder_api::sandbox::SandboxSession;
    use std::fs;
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    // With the hatch set nothing is fenced, so every probe below would pass and
    // the smoke would report SKIP -- indistinguishable from a kernel with no
    // Landlock. This binary is the only check of the real fence, so refuse.
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

    // A file OUTSIDE the future workspace, created before the fence is
    // applied. It is a sibling of the workspace under /tmp, so Landlock
    // (which allows only the workspace subpath) must deny it.
    let denied_read = PathBuf::from(format!("/tmp/landlock-smoke-denied-{pid}-{nanos}"));
    let denied_write = PathBuf::from(format!("/tmp/landlock-smoke-write-{pid}-{nanos}"));
    fs::write(&denied_read, b"secret").expect("write denied-read probe before fence");

    // Construction applies the Landlock domain to this process. The
    // workspace is a fresh temp dir the session owns and removes on Drop.
    let session =
        houyicoder_sandbox::LinuxLandlockSession::new().expect("construct landlock session");

    // Read of the sibling probe: must be denied by the kernel fence.
    let read_result = fs::read(&denied_read);
    match read_result {
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            eprintln!("PASS: denied read returned PermissionDenied");
        }
        Ok(_) => {
            eprintln!("SKIP: read of denied path succeeded; Landlock not enforced on this kernel");
            let _result = fs::remove_file(&denied_read);
            return;
        }
        Err(e) => {
            eprintln!("FAIL: denied read returned unexpected error: {e}");
            let _result = fs::remove_file(&denied_read);
            process::exit(1);
        }
    }

    // Write to a sibling path: must also be denied.
    let write_result = fs::write(&denied_write, b"x");
    match write_result {
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            eprintln!("PASS: denied write returned PermissionDenied");
        }
        Ok(_) => {
            eprintln!("FAIL: write to denied path succeeded; Landlock did not fence the write");
            let _result = fs::remove_file(&denied_read);
            let _result = fs::remove_file(&denied_write);
            process::exit(1);
        }
        Err(e) => {
            eprintln!("FAIL: denied write returned unexpected error: {e}");
            let _result = fs::remove_file(&denied_read);
            let _result = fs::remove_file(&denied_write);
            process::exit(1);
        }
    }

    // Control: a path inside the workspace is still writable under the fence.
    let allowed = session.workspace_root().join("allowed.txt");
    match fs::write(&allowed, b"ok") {
        Ok(_) => eprintln!("PASS: workspace-internal write allowed"),
        Err(e) => {
            eprintln!("FAIL: workspace-internal write denied: {e}");
            let _result = fs::remove_file(&denied_read);
            process::exit(1);
        }
    }

    let _result = fs::remove_file(&denied_read);
    eprintln!("landlock_smoke: all assertions passed");
    // session drops here, removing the workspace temp dir.
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("landlock_smoke: skipped (not linux)");
}
