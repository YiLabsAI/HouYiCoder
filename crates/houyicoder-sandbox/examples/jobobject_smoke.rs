//! Job Object smoke binary. Constructs a WindowsJobSession (which creates a
//! Job Object and applies the extended-limit fence at construction), then
//! queries the job back through the session's safe accessor to assert the
//! resource limits landed on the kernel object: the kill-on-close tree-
//! teardown flag, the per-process CPU time cap, and the process and job
//! memory caps. Built only when the enforce feature is on (see Cargo.toml
//! required-features); the source is cfg-gated to windows so non-windows
//! builds compile a no-op main.
//!
//! Run on a Windows host:
//!   cargo run --example jobobject_smoke --features enforce
//! Prints PASS when the limits are set as expected, SKIP when Job Object
//! creation degraded to a no-op (the session reported an unfenced gap), and
//! exits non-zero on an unexpected result.

#[cfg(target_os = "windows")]
fn main() {
    use std::process;

    // With the hatch set no job is created, so job_limits returns None and the
    // smoke would report SKIP -- indistinguishable from a real creation failure.
    // This binary is the only check of the real fence, so refuse.
    if std::env::var("HOUYICODER_SANDBOX_NO_ENFORCE").is_ok_and(|v| v == "1") {
        eprintln!(
            "FAIL: HOUYICODER_SANDBOX_NO_ENFORCE=1 disables the fence this binary verifies; unset it and re-run"
        );
        process::exit(2);
    }

    // Construction applies the Job Object fence to this process. The
    // workspace is a fresh temp dir the session owns and removes on Drop.
    let session =
        houyicoder_sandbox::WindowsJobSession::new().expect("construct job object session");

    // A None return means construction degraded to a no-op (Job Object
    // creation failed); the session already emitted an audit line, so SKIP.
    let limits = match session.job_limits() {
        Some(Ok(l)) => l,
        Some(Err(e)) => {
            eprintln!("FAIL: query job limits: {e}");
            process::exit(1);
        }
        None => {
            eprintln!("SKIP: job object not created; session degraded to no-op");
            return;
        }
    };

    let mut pass = true;
    if !limits.kill_on_close {
        eprintln!("FAIL: kill-on-close flag not set on the job");
        pass = false;
    } else {
        eprintln!("PASS: kill-on-close set (tree teardown armed)");
    }
    // cpu_secs default is 30s -> 300_000_000 100ns-units.
    let expected_cpu = 30 * 10_000_000;
    if limits.cpu_100ns != expected_cpu {
        eprintln!(
            "FAIL: per-process CPU time cap is {} 100ns-units, expected {expected_cpu}",
            limits.cpu_100ns
        );
        pass = false;
    } else {
        eprintln!(
            "PASS: per-process CPU time cap set to {} 100ns-units",
            limits.cpu_100ns
        );
    }
    // as_bytes default is 2 GiB. Annotated usize: 2 * 1024^3 overflows the
    // default i32 literal type (2 GiB > i32::MAX), and this example only
    // compiles on windows, so the overflow surfaces only there.
    let two_gib: usize = 2 * 1024 * 1024 * 1024;
    if limits.process_memory != two_gib as usize {
        eprintln!(
            "FAIL: process memory cap is {}, expected {two_gib}",
            limits.process_memory
        );
        pass = false;
    } else {
        eprintln!(
            "PASS: process memory cap set to {} bytes",
            limits.process_memory
        );
    }
    if limits.job_memory != two_gib as usize {
        eprintln!(
            "FAIL: job memory cap is {}, expected {two_gib}",
            limits.job_memory
        );
        pass = false;
    } else {
        eprintln!("PASS: job memory cap set to {} bytes", limits.job_memory);
    }

    if pass {
        eprintln!("jobobject_smoke: all assertions passed");
    } else {
        process::exit(1);
    }
    // session drops here, closing the job handle and removing the workspace.
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("jobobject_smoke: skipped (not windows)");
}
