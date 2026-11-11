//! Two live processes writing one permission scope file must not lose a rule.
//!
//! The store is shared by every process in the project, so a write claims a
//! cross-process lock and replaces the file through a temp name that belongs
//! to the writer. These tests run real child processes because an in-process
//! imitation of a file lock proves nothing about two processes contending for
//! one store.

#![cfg(unix)]
#![expect(clippy::disallowed_methods, reason = "the test starts its own binary")]

use std::env;
use std::env::current_exe;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::read;
use std::fs::read_dir;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::process::id;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::channel;
use std::thread;
use std::thread::sleep;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use houyicoder_permission::Effect;
use houyicoder_permission::FileRuleStore;
use houyicoder_permission::Rule;
use houyicoder_permission::RuleContent;
use houyicoder_permission::RuleStore;

/// The lock file the store claims beside the project scope file.
const LOCK_NAME: &str = ".project.json.lock";
/// Markers a child uses to coordinate with the parent (ready to race, may
/// proceed). Polled files keep the two processes off any shared clock.
const READY_MARKER: &str = "child.ready";
const RELEASE_MARKER: &str = "child.release";
/// Environment naming the child's role and its store root.
const ROLE_ENV: &str = "HOUYICODER_PERMISSION_TEST_ROLE";
const ROOT_ENV: &str = "HOUYICODER_PERMISSION_TEST_ROOT";
/// A blocked write is expected to stay blocked for the probe window; a write
/// that returns inside it did not wait on the lock.
const BLOCK_PROBE: Duration = Duration::from_millis(300);
/// Generous deadline for a child to finish its side.
const CHILD_DEADLINE: Duration = Duration::from_secs(30);

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = env::temp_dir().join(format!("houyi-permission-{tag}-{}-{nanos}", id()));
    remove_dir_all(&root).ok();
    create_dir_all(&root).expect("create the store root");
    root
}

fn store(root: &Path) -> FileRuleStore {
    FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    )
}

fn rule(prefix: &str) -> Rule {
    Rule::with_content(
        "bash",
        RuleContent::Prefix(prefix.to_string()),
        Effect::Allow,
    )
    .expect("a rule")
}

/// Run the child half when the environment names one. Returns true in a
/// child, which then ends its test immediately: the child's stdout is not
/// the assertion point, the durable file is.
fn run_child_role() -> bool {
    let Ok(role) = env::var(ROLE_ENV) else {
        return false;
    };
    let root = PathBuf::from(env::var(ROOT_ENV).expect("child store root"));
    match role.as_str() {
        "hold-lock" => {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(root.join(LOCK_NAME))
                .expect("open the store lock");
            file.lock().expect("hold the store lock");
            write(root.join(READY_MARKER), b"").expect("signal ready");
            wait_for_file(&root.join(RELEASE_MARKER));
            drop(file);
        }
        "write" => {
            write(root.join(READY_MARKER), b"").expect("signal ready");
            wait_for_file(&root.join(RELEASE_MARKER));
            store(&root).add(&rule("child")).expect("child add");
        }
        other => panic!("unknown child role {other}"),
    }
    true
}

fn wait_for_file(path: &Path) -> bool {
    let deadline = Instant::now() + CHILD_DEADLINE;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        sleep(Duration::from_millis(2));
    }
    false
}

fn start_child(test_name: &str, role: &str, root: &Path) -> Child {
    Command::new(current_exe().expect("the test binary path"))
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(ROLE_ENV, role)
        .env(ROOT_ENV, root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the child process")
}

fn wait_child(child: &mut Child) {
    let started = Instant::now();
    let status = child.wait().expect("the child exits");
    assert!(
        status.success(),
        "the child failed after {:?}",
        started.elapsed()
    );
}

/// A rule write either lands or reports the failure: it must wait for the
/// store lock rather than proceed under a lock another process holds. The
/// child holds the lock until told to release, so an add that returns inside
/// the probe window wrote without the lock.
#[test]
fn test_add_waits_for_lock() {
    if run_child_role() {
        return;
    }
    let root = temp_root("held-lock");
    let mut child = start_child("test_add_waits_for_lock", "hold-lock", &root);
    assert!(
        wait_for_file(&root.join(READY_MARKER)),
        "the child must take the lock before the parent writes"
    );
    let store = store(&root);
    let (tx, rx) = channel();
    let writer = thread::spawn(move || {
        let outcome = store.add(&rule("alpha"));
        tx.send(outcome).expect("the writer reports its outcome");
    });
    assert!(
        matches!(rx.recv_timeout(BLOCK_PROBE), Err(RecvTimeoutError::Timeout)),
        "the add must wait while another process holds the store lock"
    );
    write(root.join(RELEASE_MARKER), b"").expect("release the child");
    let outcome = rx
        .recv_timeout(CHILD_DEADLINE)
        .expect("the add completes once the lock is free");
    assert!(outcome.is_ok(), "the add must land after the release");
    writer.join().expect("the writer thread ends");
    wait_child(&mut child);
    let text = read_to_string(root.join("project.json")).expect("the scope file is readable");
    assert!(
        text.contains("alpha"),
        "the released add must be on disk:\n{text}"
    );
    remove_dir_all(&root).ok();
}

/// Both processes add one rule of their own to the same scope file. Each add
/// is a read-modify-write under the store lock, so neither rule is lost and
/// the two writers leave no temp files behind.
#[test]
fn test_stores_keep_both() {
    if run_child_role() {
        return;
    }
    let root = temp_root("two-writers");
    let mut child = start_child("test_stores_keep_both", "write", &root);
    assert!(
        wait_for_file(&root.join(READY_MARKER)),
        "the child must be ready before the race starts"
    );
    let store = store(&root);
    write(root.join(RELEASE_MARKER), b"").expect("release the child");
    store.add(&rule("parent")).expect("parent add");
    wait_child(&mut child);
    let loaded = crate::store(&root).try_load().expect("load");
    let contents: Vec<String> = loaded
        .iter()
        .filter_map(|r| match &r.content {
            Some(RuleContent::Prefix(p)) => Some(p.clone()),
            _ => None,
        })
        .collect();
    assert!(
        contents.iter().any(|p| p == "parent"),
        "the parent rule must survive: {contents:?}"
    );
    assert!(
        contents.iter().any(|p| p == "child"),
        "the child rule must survive: {contents:?}"
    );
    let leftovers: Vec<String> = read_dir(&root)
        .expect("the store root lists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with('.') && name.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "a completed write must leave no temp files: {leftovers:?}"
    );
    remove_dir_all(&root).ok();
}

/// Both processes add one rule to a scope file that also carries a member this
/// build does not model. Each add is a read-modify-write under the store lock,
/// so both rules survive and the member the other build wrote is carried back
/// unchanged rather than dropped by the rewrite.
#[test]
fn test_stores_keep_unknown_field() {
    if run_child_role() {
        return;
    }
    let root = temp_root("unknown-member");
    write(
        root.join("project.json"),
        r#"{"version":2,"rules":[],"directories":[],"from_another_build":"kept"}"#,
    )
    .expect("seed the scope file");
    let mut child = start_child("test_stores_keep_unknown_field", "write", &root);
    assert!(
        wait_for_file(&root.join(READY_MARKER)),
        "the child must be ready before the race starts"
    );
    let store = store(&root);
    write(root.join(RELEASE_MARKER), b"").expect("release the child");
    store.add(&rule("parent")).expect("parent add");
    wait_child(&mut child);
    let text = read_to_string(root.join("project.json")).expect("read the scope file");
    let value: serde_json::Value = serde_json::from_str(&text).expect("the file is JSON");
    assert_eq!(
        value["from_another_build"], "kept",
        "the member this build does not model survives both writers:\n{text}"
    );
    let loaded = crate::store(&root).try_load().expect("load");
    let contents: Vec<String> = loaded
        .iter()
        .filter_map(|r| match &r.content {
            Some(RuleContent::Prefix(p)) => Some(p.clone()),
            _ => None,
        })
        .collect();
    assert!(
        contents.iter().any(|p| p == "parent"),
        "the parent rule must survive: {contents:?}"
    );
    assert!(
        contents.iter().any(|p| p == "child"),
        "the child rule must survive: {contents:?}"
    );
    remove_dir_all(&root).ok();
}

/// An add blocked behind another writer's lock must read the scope file after
/// it gets the lock, not before it waits. The scope file a blocked add lands
/// on is written while the lock is held, so an add that read before blocking
/// publishes its own rule over the one it never saw.
#[test]
fn test_add_keeps_locked_rule() {
    let root = temp_root("read-under-lock");
    let seed = temp_root("read-under-lock-seed");
    store(&seed).add(&rule("landed")).expect("seed the rule");
    let landed = read(seed.join("project.json")).expect("the seeded envelope");
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(root.join(LOCK_NAME))
        .expect("open the store lock");
    lock.lock().expect("hold the store lock");
    let (tx, rx) = channel();
    let writer = {
        let store = store(&root);
        thread::spawn(move || {
            let outcome = store.add(&rule("alpha"));
            tx.send(outcome).expect("report the add");
        })
    };
    assert!(
        matches!(rx.recv_timeout(BLOCK_PROBE), Err(RecvTimeoutError::Timeout)),
        "the add must wait while the lock is held"
    );
    write(root.join("project.json"), &landed).expect("the locked writer lands a rule");
    drop(lock);
    let outcome = rx
        .recv_timeout(CHILD_DEADLINE)
        .expect("the add completes once the lock is free");
    assert!(outcome.is_ok(), "the add must land after the release");
    writer.join().expect("the writer thread ends");
    let loaded = crate::store(&root).try_load().expect("load");
    let contents: Vec<String> = loaded
        .iter()
        .filter_map(|r| match &r.content {
            Some(RuleContent::Prefix(p)) => Some(p.clone()),
            _ => None,
        })
        .collect();
    assert!(
        contents.iter().any(|p| p == "landed"),
        "a rule landed under the lock must survive the waiting add: {contents:?}"
    );
    assert!(
        contents.iter().any(|p| p == "alpha"),
        "the waiting add must land: {contents:?}"
    );
    remove_dir_all(&root).ok();
    remove_dir_all(&seed).ok();
}
