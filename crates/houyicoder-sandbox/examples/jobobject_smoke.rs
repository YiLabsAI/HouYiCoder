//! Job Object smoke binary. Runs commands through a WindowsJobSession and
//! verifies the per-command fence: a command that outlives its wall budget is
//! ended whole, tree included, and a descendant started by a command that
//! returns at once stops with the call. With the enforce feature off, or off
//! windows, the main is a no-op; see Cargo.toml required-features.
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
use std::env::current_dir;
#[cfg(target_os = "windows")]
use std::env::var;
#[cfg(target_os = "windows")]
use std::fs::{metadata, remove_file};
#[cfg(target_os = "windows")]
use std::path::Path;
#[cfg(target_os = "windows")]
use std::process;
#[cfg(target_os = "windows")]
use std::thread::sleep;
#[cfg(target_os = "windows")]
use std::time::Duration;
#[cfg(target_os = "windows")]
use std::time::Instant;
#[cfg(target_os = "windows")]
use tokio::runtime::{Builder, Runtime};

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
    // The exec path awaits on timers and on the blocking pool, so the probe
    // brings its own runtime instead of parking the thread in place.
    let runtime = match Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("FAIL: build the probe runtime: {e}");
            process::exit(1);
        }
    };
    check_tree_ends_with_call(&session, &runtime);
    check_descendant_ends_with_call(&session, &runtime);
}

/// A command whose tree sleeps past a short wall budget must be ended by the
/// timeout, tree included: the second sleeper is a child of the first, so a
/// fence that reached only the direct child would let it run on.
#[cfg(target_os = "windows")]
fn check_tree_ends_with_call(session: &WindowsJobSession, runtime: &Runtime) {
    let config = ExecConfig {
        wall_timeout_ms: 3_000,
        ..ExecConfig::default()
    };
    let command = "cmd /c ping -n 30 127.0.0.1 & cmd /c ping -n 30 127.0.0.1";
    let started = Instant::now();
    let outcome = runtime.block_on(session.exec_with_config(command, config));
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

/// A descendant started by the command at once belongs to the job from its
/// creation, so the job's close must end it even though it outlives the
/// command that started it. The descendant writes one line a second, so a
/// reader can tell a stopped tree from one still running.
#[cfg(target_os = "windows")]
fn check_descendant_ends_with_call(session: &WindowsJobSession, runtime: &Runtime) {
    let marker = current_dir().expect("current dir").join("escaped.txt");
    drop(remove_file(&marker));
    let config = ExecConfig {
        wall_timeout_ms: 5_000,
        ..ExecConfig::default()
    };
    let command = "start /b ping -n 30 127.0.0.1 > escaped.txt";
    match runtime.block_on(session.exec_with_config(command, config)) {
        Ok(result) if result.exit_code == Some(0) => {}
        Ok(result) => {
            eprintln!("FAIL: the starting command exited {:?}", result.exit_code);
            process::exit(1);
        }
        Err(e) => {
            eprintln!("FAIL: start the descendant: {e}");
            process::exit(1);
        }
    }
    if !wait_for_output(&marker, Duration::from_secs(3)) {
        // Nothing was observed, which says as much about the probe as about
        // the fence: a shell that gives the descendant its inherited pipe
        // instead of the redirected file leaves no trace here. The exit code
        // keeps that apart from a fence that let a descendant run on.
        eprintln!(
            "INCONCLUSIVE: the descendant wrote nothing to {}; the probe observed nothing to end",
            marker.display()
        );
        process::exit(3);
    }
    let before = metadata(&marker).map(|m| m.len()).unwrap_or(0);
    sleep(Duration::from_secs(3));
    let after = metadata(&marker).map(|m| m.len()).unwrap_or(0);
    drop(remove_file(&marker));
    if after > before {
        eprintln!(
            "FAIL: the descendant wrote {after} bytes after the call returned, up from {before}; it outlived its job"
        );
        process::exit(1);
    }
    eprintln!("PASS: the descendant stopped with the call ({after} bytes, then stable)");
}

/// Wait until the file holds bytes, answering whether it did.
#[cfg(target_os = "windows")]
fn wait_for_output(path: &Path, budget: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < budget {
        if metadata(path).is_ok_and(|m| m.len() > 0) {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    false
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("jobobject_smoke: skipped (not windows)");
}
