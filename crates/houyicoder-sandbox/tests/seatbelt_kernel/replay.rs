//! Replay of the commands a real session failed on, run under the session's own
//! shell and profile. Each entry is a command the agent actually issued, so the
//! suite is the regression net for the whole class rather than for one repaired
//! path.

use super::Fence;
use houyicoder_sandbox::command_shell;
use std::fs::remove_file;
use std::fs::write;

/// Commands that failed inside the session, each paired with the marker its
/// success prints. They run under the shell the session uses, which is the
/// property the dialect cases exist to pin: the same lines abort under a login
/// shell that reads a leading === as a command lookup, treats status as
/// read-only, and refuses an unmatched glob.
#[cfg(target_os = "macos")]
#[test]
fn test_session_shell_failures() {
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
        // A glob the shell refuses is reported in its own words, which is what
        // the dialect cases exist to keep out of the session.
        assert!(
            !out.stderr.contains("no matches found"),
            "{label}: the shell must hand the pattern on rather than refuse it. {}",
            out.detail()
        );
    }
}

/// The profile-level failures: the shared temp root, process substitution
/// through it, and the public CA bundle.
#[cfg(target_os = "macos")]
#[test]
fn test_session_path_failures() {
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
