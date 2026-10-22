//! Replay of the commands a real session failed on, run under the rendered
//! profile. Each entry is a command the agent actually issued and the fence
//! refused, so the suite is the regression net for the whole class rather than
//! for one repaired path.

use super::Fence;
use std::fs::remove_file;
use std::fs::write;

/// The commands that failed inside the session, each paired with the failure it
/// produced. A regression here means a session is failing on its own shell
/// dialect or on a public file again.
#[cfg(target_os = "macos")]
#[test]
fn test_session_failures_stay_fixed() {
    let f = Fence::new("session-replay");
    // The grep case needs a file to match, and the probe runs in the fixture
    // workspace rather than the checkout.
    write(f.workspace.join("probe.rs"), "fn needle() {}\n").expect("write probe file");
    let cases = [
        (
            "equals-expansion",
            "echo ===; echo ====; echo after",
            "a leading === is text, not a command lookup",
        ),
        (
            "status-variable",
            "status=0; echo status=$status",
            "status is an ordinary variable, not a read-only one",
        ),
        (
            "unmatched-glob",
            "ls scripts/*.json 2>/dev/null; echo after",
            "an unmatched glob does not abort the command line",
        ),
        (
            "grep-include-glob",
            "grep -rn needle --include=*.rs .",
            "a quoted glob after --include reaches grep intact",
        ),
        (
            "shared-temp-write",
            "printf x > /tmp/houyi-replay-probe && head -c 1 /tmp/houyi-replay-probe",
            "a tool that hard-codes the shared temp root can write there",
        ),
        (
            "process-substitution",
            // The probe shell is sh; the session runs bash.
            "bash -c 'diff <(echo a) <(echo a) && echo same'",
            "the system diff can stage pipe input",
        ),
        (
            "public-ca-read",
            "head -c 32 /etc/ssl/cert.pem > /dev/null && echo ca-ok",
            "a TLS client can load the public CA bundle",
        ),
    ];
    for (label, command, why) in cases {
        let out = f.probe(label, command);
        assert!(out.allowed(), "{label}: {why}. {}", out.detail());
    }
    let _cleanup = remove_file("/tmp/houyi-replay-probe");
}
