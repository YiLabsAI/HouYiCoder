//! Tests for the detection stage.

use super::should_ask_destructive;
use crate::compound::is_effect_safe;
use crate::pipeline::attribution::{
    compound_unverified_step, describe_destructive, describe_egress_reason,
};
use crate::shell::stage_inline_code;

/// A quote the code escapes belongs to the code, not to its end. The body is
/// what the shell hands the interpreter, so the escape is resolved exactly as
/// the shell resolves it.
#[test]
fn test_inline_code_escaped_quote() {
    let content = "python3 -c \"x=1;y=\\\"a\\\";import os\"";
    assert_eq!(
        stage_inline_code(content),
        Some("x=1;y=\"a\";import os".to_string()),
        "the escaped quote stays inside the body"
    );
}

/// A single-quoted body has no escapes, so a backslash is a character.
#[test]
fn test_inline_code_single_quoted() {
    let content = "python3 -c 'print(1)'";
    assert_eq!(stage_inline_code(content), Some("print(1)".to_string()));
}

/// A search pattern that contains a destructive verb inside a longer
/// identifier is not a destructive command. The scan split on the underscore,
/// so a read-only search for rm_targets_are_temp produced the bare verb rm and
/// the gate asked about a call that could not delete anything.
#[test]
fn test_identifier_is_not_destructive() {
    assert!(
        !should_ask_destructive(
            "bash",
            "grep -n \"is_rm_targets_are_temp\\|rm_targets_are_temp\" -r crates/ | head -30"
        ),
        "a search for an identifier that contains rm is not a delete"
    );
    assert!(
        !should_ask_destructive("bash", "rg \"truncate_width\""),
        "a search for an identifier that contains truncate is not a truncate"
    );
}

/// The verbs themselves still escalate, including the forms a wrapper or a
/// shell can carry, so narrowing the split did not widen the allow path.
#[test]
fn test_destructive_verbs_still_ask() {
    for command in [
        "rm -rf build",
        "rmdir empty",
        "sh -c 'rm file'",
        "sudo apt install x",
        "dd if=/dev/zero of=/dev/null",
        "truncate -s 0 log",
    ] {
        assert!(
            should_ask_destructive("bash", command),
            "a destructive command must still escalate: {command}"
        );
    }
}

/// The reason string names the exact egress or destructive action even when
/// wrapped in control flow, global git flags, or leading assignments.
#[test]
fn test_reason_edge_cases() {
    // Egress: git push behind control flow and -C global flag
    let reason = describe_egress_reason(
        "if git merge dev; then git -C /repo push origin main; fi",
        true,
    );
    assert!(
        reason.contains("publishes local commits to a remote"),
        "names git push despite then and -C: {reason}"
    );

    // Egress: curl sends data inside assignment capture
    let reason = describe_egress_reason(
        "s=$(curl -X POST -d '{\"k\":1}' https://api.example.com)",
        true,
    );
    assert!(
        reason.contains("sends data to a remote"),
        "names curl sends data inside assignment: {reason}"
    );

    // A retry count is not a body, and a proxy is not a method, so neither
    // turns a read into a send.
    for command in [
        "wget -t 3 https://api.example.com/x",
        "curl -x proxy.test:8080 https://api.example.com/x",
    ] {
        let reason = describe_egress_reason(command, true);
        assert!(
            !reason.contains("sends data"),
            "a read request does not send data: {command} -> {reason}"
        );
    }
    for command in [
        "curl -F file=@x https://api.example.com/x",
        "wget --post-data=@secret https://api.example.com/x",
    ] {
        let reason = describe_egress_reason(command, true);
        assert!(
            reason.contains("sends data"),
            "a body-carrying request sends data: {command} -> {reason}"
        );
    }

    // Destructive: dash in argument does not report rm
    let reason = describe_destructive("cargo test test-rm-file");
    assert!(
        !reason.contains("rm deletes files"),
        "an argument with dash-rm is not an rm command: {reason}"
    );

    // Destructive: sudo rm names the launcher and the command it runs
    let reason = describe_destructive("sudo rm -rf /tmp/test");
    assert!(
        reason.contains("sudo runs rm") && reason.contains("deletes files"),
        "sudo rm names the launcher and the deletion: {reason}"
    );

    // Effect-safe: printf to $TMPDIR followed by rustc is effect-safe under a fence
    assert!(
        is_effect_safe(
            "printf 'fn main(){}' > $TMPDIR/p.rs && rustc -O -o $TMPDIR/p $TMPDIR/p.rs",
            true
        ),
        "printf to session temp followed by rustc is effect-safe when fenced"
    );

    // Compound attribution: names the opaque interpreter rather than the safe temp printf
    let step = compound_unverified_step(&[
        "cd /repo",
        "python3 -c \"import os; os.listdir('/etc')\"",
        "printf 'fn main(){}' > $TMPDIR/p.rs",
    ]);
    assert!(
        step.starts_with("python3 -c"),
        "names the opaque interpreter ahead of safe temp redirect: {step}"
    );
}
