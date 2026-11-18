//! Tests for this module. The unix parent gates the module, so the file
//! carries no platform conditional.

use std::process::Command;

use crate::desktop::clipboard::CopyOutcome;

use super::{CopyInputs, decide_outcome, run_utility_unix, wait_bounded};

/// Over SSH the tmux paste buffer still works: it is on the remote tmux
/// server, and -w bridges to the outer terminal.
#[test]
fn test_ssh_tmux_wins() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: true,
            in_tmux: true,
            tmux_buffer_ok: true,
            native_ok: false,
        }),
        CopyOutcome::TmuxBufferCopied
    );
}

/// Over SSH with no tmux no native path applies: a local utility would write
/// the remote clipboard, so the caller gets the unavailability and falls
/// back to the terminal escape.
#[test]
fn test_ssh_without_tmux_unavailable() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: true,
            in_tmux: false,
            tmux_buffer_ok: false,
            native_ok: false,
        }),
        CopyOutcome::NoNativePath
    );
}

/// Over SSH with tmux but a failed buffer load, no native path applies
/// either, so the caller falls back to the escape.
#[test]
fn test_ssh_buffer_failure_unavailable() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: true,
            in_tmux: true,
            tmux_buffer_ok: false,
            native_ok: false,
        }),
        CopyOutcome::NoNativePath
    );
}

/// A local native success is the whole copy: the caller adds no escape.
#[test]
fn test_local_native_success() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: false,
            in_tmux: false,
            tmux_buffer_ok: false,
            native_ok: true,
        }),
        CopyOutcome::NativeCopied
    );
}

/// A local native failure keeps its identity so the caller can tell "the
/// tool failed and we fell back" from "no tool exists".
#[test]
fn test_local_native_failure() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: false,
            in_tmux: false,
            tmux_buffer_ok: false,
            native_ok: false,
        }),
        CopyOutcome::NativeFailed
    );
}

/// The tmux paste buffer is the whole copy when it answers, even locally,
/// so the native utility is not run a second time.
#[test]
fn test_tmux_buffer_alone() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: false,
            in_tmux: true,
            tmux_buffer_ok: true,
            native_ok: false,
        }),
        CopyOutcome::TmuxBufferCopied
    );
}

/// A local tmux session whose buffer load failed still reaches the native
/// path, and its failure is named as one.
#[test]
fn test_local_buffer_failure_native() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: false,
            in_tmux: true,
            tmux_buffer_ok: false,
            native_ok: false,
        }),
        CopyOutcome::NativeFailed
    );
}

/// A utility that exits cleanly reports success through the bounded wait.
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding, not an engine spawn"
)]
#[test]
fn test_utility_exit_success() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .expect("spawn sh");
    assert!(wait_bounded(&mut child), "a clean exit is a success");
}

/// A utility that exits non-zero reports failure through the bounded wait.
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding, not an engine spawn"
)]
#[test]
fn test_utility_exit_failure() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("exit 1")
        .spawn()
        .expect("spawn sh");
    assert!(!wait_bounded(&mut child), "a non-zero exit is a failure");
}

/// A utility that takes a moment exercises the poll loop: the first
/// try_wait answers None, the deadline check passes, and a later poll
/// finds the child exited.
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding, not an engine spawn"
)]
#[test]
fn test_wait_bounded_polls() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("sleep 0.01")
        .spawn()
        .expect("spawn sh");
    assert!(wait_bounded(&mut child), "the child exits within the bound");
}

/// The utility runner feeds the text on standard input: a command that
/// reads its stdin to the null device demonstrates the pipe.
#[test]
fn test_utility_feeds_stdin() {
    let ok = run_utility_unix("/bin/sh", &["-c", "cat > /dev/null"], "copied");
    assert!(ok.is_ok(), "a clean utility run reports success");
}

/// A utility that cannot start is an error rather than a hang, so a host
/// missing the tool falls back rather than freezing.
#[test]
fn test_utility_reports_missing() {
    assert!(
        run_utility_unix("/nonexistent/pbcopy", &[], "copied").is_err(),
        "a missing utility is reported"
    );
}
