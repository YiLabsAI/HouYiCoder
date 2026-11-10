//! Programs a launcher runs, replayed through the gate. The words after a
//! launcher's own options decide which program runs, so each case pins that
//! the scan reaches that program rather than stopping at the launcher.

use super::*;
use crate::decision::{Decision, Outcome};

fn gate() -> DefaultModeGate {
    DefaultModeGate::with_mode(PermissionMode::Auto)
}

/// Launchers whose own name says nothing about the deletion they run.
const LAUNCHED_DELETION: &[&str] = &[
    "arch -arm64 rm -rf build",
    "busybox rm -rf build",
    "caffeinate -t 60 rm -rf build",
    "chroot /tmp/root rm -rf /build",
    "env LANG=C rm -rf build",
    "fakeroot rm -rf build",
    "nice -n 5 rm -rf build",
    "nohup rm -rf build",
    "proxychains rm -rf build",
    "script -q /dev/null rm -rf build",
    "strace -f rm -rf build",
    "taskset -c 0 rm -rf build",
    "timeout 30 rm -rf build",
    "watch -n 5 rm -rf build",
    "xvfb-run rm -rf build",
];

/// The launcher names the program it elevates, and the prompt names both.
const ESCALATED: &[(&str, &str)] = &[
    (
        "sudo apt install jq",
        "sudo runs a command with elevated privileges",
    ),
    (
        "doas apt install jq",
        "doas runs a command with elevated privileges",
    ),
    (
        "pkexec apt install jq",
        "pkexec runs a command with elevated privileges",
    ),
    (
        "sudo -u root truncate -s 0 /tmp/x",
        "sudo runs truncate, which shrinks or empties a file",
    ),
    (
        "su -c 'rm -rf /tmp/x'",
        "su runs rm, which deletes files or directories",
    ),
];

#[test]
fn test_launcher_deletion_asks() {
    let gate = gate();
    for command in LAUNCHED_DELETION {
        match gate.decide(&bash_req(command)) {
            Decision::Ask(reason) => {
                assert_eq!(reason.validator, "destructive_command", "{command}");
                assert!(
                    reason.detail.contains("rm deletes files or directories"),
                    "the prompt must name the deletion: {command} -> {}",
                    reason.detail
                );
            }
            other => panic!("launched deletion must ask: {command} -> {other:?}"),
        }
    }
}

#[test]
fn test_launcher_escalation_names_privilege() {
    let gate = gate();
    for (command, detail) in ESCALATED {
        match gate.decide(&bash_req(command)) {
            Decision::Ask(reason) => {
                assert_eq!(reason.validator, "destructive_command", "{command}");
                assert!(
                    reason.detail.contains(detail),
                    "the prompt must name the privilege: {command} -> {}",
                    reason.detail
                );
            }
            other => panic!("escalation must ask: {command} -> {other:?}"),
        }
    }
}

/// A signal inside the code a launcher runs still reaches the signal check.
#[test]
fn test_launcher_signal_asks() {
    let gate = gate();
    for command in [
        "arch -arm64 python3 -c 'import os; os.kill(1, 9)'",
        "script -q /dev/null python3 -c 'import os; os.kill(1, 9)'",
        "strace -f python3 -c 'import os; os.kill(1, 9)'",
    ] {
        assert_eq!(
            gate.decide(&bash_req(command)).outcome(),
            Outcome::Ask,
            "launched signal must ask: {command}"
        );
    }
}

/// A command-position substitution names the program at run time, so the
/// stage is not one the gate can vouch for.
#[test]
fn test_substitution_program_asks() {
    let gate = gate();
    for command in [
        "$(which rm) -rf build",
        "cd /tmp && $(which rm) -rf build",
        "$(which git) push origin dev",
        "$(printf rm) -rf build",
    ] {
        assert_eq!(
            gate.decide(&bash_req(command)).outcome(),
            Outcome::Ask,
            "substitution program must ask: {command}"
        );
    }
}

/// A substitution filling an assignment is a value, not a program, so a
/// capture that only reads stays local.
#[test]
fn test_capture_value_stays_local() {
    let gate = gate();
    for command in [
        "cd /Users/von/workspace/hicoder && h=$(git rev-parse --short HEAD) && echo $h",
        "cd /Users/von/workspace/hicoder && s=$(git log -1 --format=%s) && printf '%s' \"$s\"",
    ] {
        assert_eq!(
            gate.decide(&bash_req(command)).outcome(),
            Outcome::Allow,
            "captured value must not ask: {command}"
        );
    }
}
