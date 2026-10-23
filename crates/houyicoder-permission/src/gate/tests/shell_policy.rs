//! What a fenced session may do without a prompt, and what it may not.
//!
//! The fence decides where a write lands, so the gate only has to exclude
//! constructs that change what the kernel is asked to open: a rebound session
//! temporary directory, a traversal out of it, a protected marker, or a
//! preceding command that can plant a link the later write would follow.

use super::*;
use crate::consent::{ConsentStore, InMemoryConsentStore, args_key};
use crate::decision::Outcome;
use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
use std::env::temp_dir;
use std::fs::{create_dir, remove_dir_all};
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let path = temp_dir().join(format!(
            "hy-shell-policy-{}-{}",
            id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        create_dir(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        remove_dir_all(&self.0).expect("remove owned test directory");
    }
}

struct TestFence {
    directory: PathBuf,
    active: bool,
}

impl Containment for TestFence {
    fn coverage(&self) -> Coverage {
        if self.active {
            Coverage::Fenced {
                writable_roots: vec![self.directory.clone()],
            }
        } else {
            Coverage::Unfenced
        }
    }

    fn would_block(&self, _: SideEffect) -> Option<String> {
        None
    }
}

fn gate(directory: &TestDir, active: bool) -> DefaultModeGate {
    DefaultModeGate::with_mode(PermissionMode::Auto)
        .with_trusted_hosts()
        .with_containment(Arc::new(TestFence {
            directory: directory.0.clone(),
            active,
        }))
}

fn outcome(gate: &DefaultModeGate, command: &str) -> Outcome {
    gate.decide(&bash_req(command)).outcome()
}

#[test]
fn test_fenced_temp_output_allows() {
    let directory = TestDir::new();
    let gate = gate(&directory, true);
    for command in [
        "printf changed > \"$TMPDIR/result.log\"",
        "printf changed >> \"$TMPDIR/result.log\"",
        "make check > \"$TMPDIR/check.log\" 2>&1; tail -20 \"$TMPDIR/check.log\"",
        "cd /work/repo && cargo nextest run > \"$TMPDIR/nt.log\" 2>&1; echo done",
        "rm -rf \"$TMPDIR/fresh\" && mkdir -p \"$TMPDIR/fresh\"",
        "cd /work/repo && git diff --name-status a721b54a e04fef2c > \"$TMPDIR/d.txt\"",
    ] {
        assert_eq!(outcome(&gate, command), Outcome::Allow, "{command}");
    }
}

#[test]
fn test_unfenced_temp_output_asks() {
    let directory = TestDir::new();
    let gate = gate(&directory, false);
    for command in [
        "printf changed > \"$TMPDIR/result.log\"",
        "make check > \"$TMPDIR/check.log\" 2>&1",
        "rm -rf \"$TMPDIR/fresh\"",
    ] {
        assert_eq!(outcome(&gate, command), Outcome::Ask, "{command}");
    }
}

#[test]
fn test_fenced_temp_escapes_ask() {
    let directory = TestDir::new();
    let gate = gate(&directory, true);
    for command in [
        "printf changed > \"$TMPDIR/../escape\"",
        "printf changed > \"$TMPDIR/.git/config\"",
        "printf changed > \"$TMPDIR/nested/deep.log\"",
        "printf changed > \"$TMPDIR/*.log\"",
        "printf changed > /work/repo/src/main.rs",
        "printf changed > \"$TMPDIR/\"",
        "COV_DIR=TMP; COV_DIR=$COV_DIR'DIR'; getopts a \"$COV_DIR\" -a; rm -rf \"$TMPDIR/src\"",
        "name=TMP; name=$name'DIR'; pri'ntf' -v \"$name\" /work/repo; rm -rf \"$TMPDIR/src\"",
        "python3 -c 'import os;os.symlink(\"a\",\"b\")'; printf changed > \"$TMPDIR/created.log\"",
        "ln -s /work/repo/src \"$TMPDIR/link\"; printf changed > \"$TMPDIR/link\"",
        "rm -rf \"$TMPDIR/../escape\"",
        "rm -rf /work/repo/src",
    ] {
        assert_eq!(outcome(&gate, command), Outcome::Ask, "{command}");
    }
}

#[test]
fn test_origin_keeps_output_ask() {
    let directory = TestDir::new();
    let gate = gate(&directory, true);
    let temp_output = "curl -sS https://example.test/ -o \"$TMPDIR/result.log\"";
    assert_eq!(outcome(&gate, temp_output), Outcome::Ask);
    gate.add_egress_allow("curl https://example.test");
    assert_eq!(outcome(&gate, temp_output), Outcome::Allow);
    // The remembered origin answers the read-only request and nothing else.
    for command in [
        "curl -sS https://example.test/ -o /work/repo/result.log",
        "curl -sS https://example.test/ -o \"$TMPDIR/../escape\"",
        "curl --data-binary @secret https://example.test/",
        "wget https://example.test/",
        "curl -O https://example.test/file",
    ] {
        assert_eq!(outcome(&gate, command), Outcome::Ask, "{command}");
    }
}

#[test]
fn test_unfenced_origin_asks() {
    let directory = TestDir::new();
    let gate = gate(&directory, false);
    gate.add_egress_allow("curl https://example.test");
    assert_eq!(
        outcome(
            &gate,
            "curl -sS https://example.test/ -o \"$TMPDIR/result.log\""
        ),
        Outcome::Ask
    );
    assert_eq!(
        outcome(&gate, "curl -sS https://example.test/next"),
        Outcome::Allow
    );
}

#[test]
fn test_read_commands_allow() {
    let gate = DefaultModeGate::with_mode(PermissionMode::Auto).with_trusted_hosts();
    for command in [
        "cat .claude/settings.json",
        "git status --short",
        "make check 2>&1 | tail -20",
        "curl -sS https://docs.rs/ -o /dev/null",
        "wget -O - https://docs.rs/",
    ] {
        assert_eq!(outcome(&gate, command), Outcome::Allow, "{command}");
    }
}

#[test]
fn test_exact_output_consent() {
    let consent = Arc::new(InMemoryConsentStore::non_expiring());
    let gate = DefaultModeGate::with_mode(PermissionMode::Auto).with_consent(consent.clone());
    let request = bash_req("printf x > /work/repo/out.txt");
    assert_eq!(gate.decide(&request).outcome(), Outcome::Ask);
    consent.record(request.tool_name, &args_key(request.input));
    assert_eq!(gate.decide(&request).outcome(), Outcome::Allow);
    assert_eq!(
        outcome(&gate, "printf x > /work/repo/other.txt"),
        Outcome::Ask
    );
}
