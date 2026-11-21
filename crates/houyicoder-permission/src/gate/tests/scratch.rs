//! The session scratch directory as the single source of truth for a
//! temporary output path: the change of directory that leads there, the
//! separators that end its reach, and the predicates every checker shares.

use std::path::PathBuf;
use std::sync::Arc;

use crate::decision::Outcome;
use crate::egress::{egress_outputs_safe, egress_scope_list};
use houyicoder_api::sandbox::{Containment, Coverage, FenceCapability, SideEffect};

use super::*;

/// A fence that confines writes, which is what makes a scratch path safe to
/// write to without asking.
struct ScratchFence;

impl Containment for ScratchFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![PathBuf::from("/ws")],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
    fn capability(&self) -> FenceCapability {
        FenceCapability::filesystem()
    }
}

fn fenced_trusted() -> DefaultModeGate {
    DefaultModeGate::with_mode(PermissionMode::Auto)
        .with_containment(Arc::new(ScratchFence))
        .with_trusted_hosts()
}

/// The two commands from a session that asked for approval although both only
/// download into the session scratch directory.
const SESSION_DOWNLOAD: &str = r#"cd $TMPDIR && rm -rf rmcpchk && mkdir rmcpchk && cd rmcpchk && timeout 60 curl -s "https://crates.io/api/v1/crates/rmcp/3.4.0" -H "User-Agent: check" -o r.json 2>&1; echo "EXIT=$?"; ls -la r.json 2>/dev/null"#;

const SESSION_UNPACK: &str = r#"cd $TMPDIR/rmcpchk && timeout 90 curl -sL "https://static.crates.io/crates/rmcp/rmcp-3.4.0.crate" -o rmcp.crate 2>&1; echo "EXIT=$?"; ls -la rmcp.crate; tar xzf rmcp.crate 2>/dev/null; ls rmcp-3.4.0/src/transport/ 2>/dev/null"#;

#[test]
fn test_scratch_download_silent() {
    let gate = fenced_trusted();
    for command in [SESSION_DOWNLOAD, SESSION_UNPACK] {
        assert_eq!(
            gate.decide(&bash_req(command)).outcome(),
            Outcome::Allow,
            "a download into the session scratch directory needs no approval: {command}"
        );
    }
}

#[test]
fn test_scratch_origin_remembers() {
    let gate = fenced_trusted();
    let first = "cd $TMPDIR/sub && curl -s https://ci.example.com/a -o a.json";
    assert_eq!(
        egress_scope_list(first, true).as_deref(),
        Some(&["curl https://ci.example.com".to_string()][..]),
        "a scratch write still names the origin it reads"
    );
    assert_eq!(gate.decide(&bash_req(first)).outcome(), Outcome::Ask);
    gate.add_egress_allow("curl https://ci.example.com");
    let second = "cd $TMPDIR/sub && curl -s https://ci.example.com/b -o b.json";
    assert_eq!(
        gate.decide(&bash_req(second)).outcome(),
        Outcome::Allow,
        "a remembered origin answers a second scratch write"
    );
}

#[test]
fn test_scratch_removal_chain() {
    // Only an and-and joined change of directory proves the removal lands in
    // the scratch directory: a semicolon or a pipe runs the right side even
    // when the change failed or ran in a subshell.
    let gate = fenced_trusted();
    for command in [
        "cd $TMPDIR; rm -rf work",
        "cd $TMPDIR | rm -rf work",
        "cd $TMPDIR && cd && rm -rf work",
        "cd $TMPDIR && cd -- /ws && rm -rf work",
        "cd $TMPDIR && cd .. && rm -rf work",
        // A cd into a scratch subdirectory cannot prove that subdirectory is
        // not a link planted in the scratch space, so a relative removal from
        // there is held; only a removal at the scratch root itself is silent.
        "cd $TMPDIR/lnk && rm -rf crates",
        // A nested target traverses a directory that may be a link, so it is
        // held even at the scratch root.
        "cd $TMPDIR && rm -rf a/b",
        // An explicit nested scratch path is held for the same reason: the
        // removal crosses a directory that could be a planted link.
        "rm -rf \"$TMPDIR/a/b\"",
        // A parenthesised group runs in a subshell whose working directory the
        // walk cannot place, so it ends the reach before the removal.
        "cd $TMPDIR && ( cd /ws ) && rm -rf work",
    ] {
        assert_eq!(
            gate.decide(&bash_req(command)).outcome(),
            Outcome::Ask,
            "the change of directory does not reach the removal: {command}"
        );
    }
    assert_eq!(
        gate.decide(&bash_req("cd $TMPDIR && rm -rf work"))
            .outcome(),
        Outcome::Allow,
        "an and-and chain keeps the scratch removal silent"
    );
}

#[test]
fn test_scratch_output_chain() {
    // An origin outside the trusted set, so what the gate answers here is the
    // output-path check rather than the trusted-host shortcut.
    let gate = fenced_trusted();
    for command in [
        "cd $TMPDIR/rmcpchk; curl -s https://ci.example.test/x -o r.json",
        "cd $TMPDIR/rmcpchk && mkdir a; curl -s https://ci.example.test/x -o r.json",
        "cd $TMPDIR && cd .. && curl -s https://ci.example.test/x -o r.json",
        "cd $TMPDIR/rmcpchk && curl -s https://ci.example.test/x -o ../escape.json",
        "cd /tmp && curl -s https://ci.example.test/x -o r.json",
        "cd $TMPDIR/rmcpchk && ln -s /ws/f r.json && curl -s https://ci.example.test/x -o r.json",
        // A group the reader cannot see into, a background step, a second
        // operand to the change of directory, and a subshell each end the
        // reach.
        "cd $TMPDIR && { cd / && curl -s https://ci.example.test/x -o r.json; }",
        "cd $TMPDIR && { cd /ws && curl -s https://ci.example.test/x -o r.json; }",
        "cd $TMPDIR && sleep 1 & curl -s https://ci.example.test/x -o r.json",
        "cd $TMPDIR /ws && curl -s https://ci.example.test/x -o r.json",
        "cd $TMPDIR && pushd /ws && curl -s https://ci.example.test/x -o r.json",
    ] {
        assert_eq!(
            gate.decide(&bash_req(command)).outcome(),
            Outcome::Ask,
            "the output does not provably land in the scratch directory: {command}"
        );
    }
    assert_eq!(
        egress_scope_list(
            "cd $TMPDIR && pushd /ws && curl -s https://crates.io/x -o r.json",
            true
        ),
        None,
        "pushd drops the scratch working directory in scope collection too"
    );
    // A pipe runs its right side in a subshell, so a cd on the left never
    // reaches the request on the right. The trusted origin means the only
    // reason this asks is that the relative output is not proven to land in
    // the scratch directory; a walk that carried the cd across the pipe would
    // let it through.
    assert_eq!(
        gate.decide(&bash_req(
            "cd $TMPDIR/a|cat && curl -s https://crates.io/x -o r.json"
        ))
        .outcome(),
        Outcome::Ask,
        "a piped cd does not reach the request on its right"
    );
}

#[test]
fn test_scratch_unfenced_asks() {
    // Without a fence the kernel does not bound the write, so the same path
    // needs approval however it is spelled.
    let gate = DefaultModeGate::with_mode(PermissionMode::Auto).with_trusted_hosts();
    for command in [
        "curl -s https://crates.io/x -o r.json",
        "cd $TMPDIR && curl -s https://crates.io/x -o r.json",
        "curl -s https://crates.io/x -o \"$TMPDIR/sub/r.json\"",
    ] {
        assert_eq!(
            gate.decide(&bash_req(command)).outcome(),
            Outcome::Ask,
            "an unfenced write keeps asking: {command}"
        );
    }
}

/// The long output flag, the curl download-by-name flag, a change of directory
/// with two operands, a quoted path, a glob, and a protected marker each reach
/// a branch of the output-path walk. The safe forms stay silent under a fence
/// and the unsafe ones are refused.
#[test]
fn test_scratch_output_forms() {
    // A long output flag into the scratch space is safe; into a workspace path
    // it is not. The second operand form of a change of directory ends the
    // reach, so its relative output is no longer proven inside the scratch.
    assert!(egress_outputs_safe(
        "curl -s https://ci.example.test/x --output=\"$TMPDIR/r.json\"",
        true
    ));
    assert!(!egress_outputs_safe(
        "curl -s https://ci.example.test/x --output=/ws/r.json",
        true
    ));
    // curl -O downloads by the remote name into the working directory, which
    // the walk cannot place, so it is refused even after a scratch change.
    assert!(!egress_outputs_safe(
        "cd $TMPDIR && curl -s -O https://ci.example.test/x",
        true
    ));
    // A change of directory carrying two operands does not establish a reach.
    assert!(!egress_outputs_safe(
        "cd $TMPDIR /ws && curl -s https://ci.example.test/x -o r.json",
        true
    ));
    // A quoted scratch path, a glob, and a protected marker.
    assert!(egress_outputs_safe(
        "cd $TMPDIR && curl -s https://ci.example.test/x -o \"r.json\"",
        true
    ));
    assert!(!egress_outputs_safe(
        "cd $TMPDIR && curl -s https://ci.example.test/x -o 'r*.json'",
        true
    ));
    assert!(!egress_outputs_safe(
        "cd $TMPDIR && curl -s https://ci.example.test/x -o .git/config",
        true
    ));
    // A top-level pipe runs its right side in a subshell, so a change of
    // directory on the left does not reach the request on the right.
    assert!(!egress_outputs_safe(
        "cd $TMPDIR | curl -s https://ci.example.test/x -o r.json",
        true
    ));
}
