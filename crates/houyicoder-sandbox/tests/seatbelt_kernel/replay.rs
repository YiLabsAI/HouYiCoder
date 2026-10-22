//! The commands a real session failed on, run under the session's shell and the
//! rendered profile. Each one is a command the agent issued and the session
//! refused; the tests below fail if any of them stops working.

use super::Fence;
use houyicoder_sandbox::command_shell;
use std::fs::remove_file;
use std::fs::write;

/// The shell constructs the session refused to run: a leading === read as a
/// command lookup, status treated as a read-only variable, and an unmatched
/// glob aborting the line. Each command prints a marker after the construct, so
/// a shell that rejects the line leaves the marker out.
#[cfg(target_os = "macos")]
#[test]
fn test_bash_constructs_run() {
    let f = Fence::new("session-replay");
    let shell = command_shell();
    // The grep case needs a pattern with no match, so the glob has to reach
    // grep unexpanded: a matching file would remove the condition.
    write(f.workspace.join("probe.txt"), "nothing here\n").expect("write probe file");
    let cases = [
        (
            "equals-expansion",
            "echo ===; echo ====; echo equals-after",
            "equals-after",
            "a leading === is text, not a command lookup",
        ),
        (
            "status-variable",
            "status=0; echo status=$status",
            "status=0",
            "status is an ordinary variable, not a read-only one",
        ),
        (
            "unmatched-glob",
            "ls scripts/*.json; echo glob-after",
            "glob-after",
            "an unmatched glob does not abort the command line",
        ),
        (
            "grep-include-glob",
            "grep -rn needle --include=*.rs .; echo grep-after",
            "grep-after",
            "an unexpanded glob after --include reaches grep intact",
        ),
    ];
    for (label, command, marker, why) in cases {
        let out = f.probe_via(label, &shell, command);
        assert!(
            out.stdout.contains(marker),
            "{label}: {why}. {}",
            out.detail()
        );
        // A refused glob is reported in the shell's own words, which no command
        // in the session should have to read.
        assert!(
            !out.stderr.contains("no matches found"),
            "{label}: the shell must hand the pattern on rather than refuse it. {}",
            out.detail()
        );
    }
}

/// The paths the session could not reach: the shared temp root a tool
/// hard-codes, process substitution through it, and the public CA bundle.
#[cfg(target_os = "macos")]
#[test]
fn test_shared_paths_work() {
    let f = Fence::new("session-replay-path");
    let shell = command_shell();
    let cases = [
        (
            "shared-temp-write",
            "printf x > /tmp/replay-probe && head -c 1 /tmp/replay-probe",
            "a tool that hard-codes the shared temp root can write there",
        ),
        (
            "process-substitution",
            "diff <(echo a) <(echo a) && echo same",
            "the system diff can stage pipe input",
        ),
        (
            "public-ca-read",
            "head -c 32 /etc/ssl/cert.pem > /dev/null && echo ca-ok",
            "a TLS client can load the public CA bundle",
        ),
    ];
    for (label, command, why) in cases {
        // The session's shell, so process substitution is available.
        let out = f.probe_via(label, &shell, command);
        assert!(out.allowed(), "{label}: {why}. {}", out.detail());
    }
    let _cleanup = remove_file("/tmp/replay-probe");
}
