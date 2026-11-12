//! Job Object smoke binary. Runs one command through a WindowsJobSession and
//! verifies the per-command fence: a command that outlives its wall budget is
//! ended whole, tree included. With the enforce feature off, or off windows,
//! the main is a no-op; see Cargo.toml required-features.
//!
//! Run on a Windows host:
//!   cargo run --example jobobject_smoke --features enforce

#[cfg(target_os = "windows")]
use houyicoder_api::sandbox::FenceStatus;
#[cfg(target_os = "windows")]
use houyicoder_api::sandbox::SandboxSession;
#[cfg(target_os = "windows")]
use houyicoder_context::ExecConfig;
#[cfg(target_os = "windows")]
use houyicoder_sandbox::WindowsJobSession;
#[cfg(target_os = "windows")]
use pollster::block_on;
#[cfg(target_os = "windows")]
use std::env::current_dir;
#[cfg(target_os = "windows")]
use std::env::var;
#[cfg(target_os = "windows")]
use std::process;
#[cfg(target_os = "windows")]
use std::time::Duration;
#[cfg(target_os = "windows")]
use std::time::Instant;

#[cfg(target_os = "windows")]
fn main() {
    // The hatch disables the very fence this binary verifies, so refuse
    // rather than report a vacuous pass.
    if var("HOUYICODER_SANDBOX_NO_ENFORCE").is_ok_and(|v| v == "1") {
        eprintln!(
            "FAIL: the no-enforce hatch disables the fence this binary verifies; unset it and re-run"
        );
        process::exit(2);
    }

    let cwd = current_dir().expect("current dir");
    let session = match WindowsJobSession::new_in_cwd(&cwd) {
        Ok(session) => session,
        Err(e) => {
            eprintln!("FAIL: construct session: {e}");
            process::exit(1);
        }
    };
    if !matches!(session.fence_status(), FenceStatus::Enforced) {
        eprintln!(
            "SKIP: session reports {:?}; the kernel fence is not engaged here",
            session.fence_status()
        );
        return;
    }
    check_tree_ends_with_call(&session);
}

/// A command whose tree sleeps past a short wall budget must be ended by the
/// timeout, tree included: the second sleeper is a child of the first, so a
/// fence that reached only the direct child would let it run on.
#[cfg(target_os = "windows")]
fn check_tree_ends_with_call(session: &WindowsJobSession) {
    let mut config = ExecConfig::default();
    config.wall_timeout_ms = 3_000;
    let command = "cmd /c ping -n 30 127.0.0.1 & cmd /c ping -n 30 127.0.0.1";
    let started = Instant::now();
    let outcome = block_on(session.exec_with_config(command, config));
    let elapsed = started.elapsed();
    match outcome {
        Err(_timeout) => {
            if elapsed < Duration::from_secs(20) {
                eprintln!(
                    "PASS: the command tree was ended at {}ms, within the wall budget",
                    elapsed.as_millis()
                );
            } else {
                eprintln!(
                    "FAIL: the run took {}ms; the tree was not ended with the call",
                    elapsed.as_millis()
                );
                process::exit(1);
            }
        }
        Ok(result) => {
            eprintln!(
                "FAIL: the sleeper finished on its own (exit {:?}); the tree should have hit the wall timeout",
                result.exit_code
            );
            process::exit(1);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("jobobject_smoke: skipped (not windows)");
}
