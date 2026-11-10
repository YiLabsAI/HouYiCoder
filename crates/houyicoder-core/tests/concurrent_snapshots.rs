//! Two live processes snapshotting one workspace must claim distinct ids.
//!
//! The snapshot store is shared by every process working in the workspace,
//! so a new snapshot claims its directory name by creating it exclusively.
//! These tests run real child processes because the collision is two live
//! processes seeding their counters from the same on-disk scan.

#![cfg(unix)]
#![expect(clippy::disallowed_methods, reason = "the test starts its own binary")]

use std::env;
use std::env::current_exe;
use std::fs::create_dir_all;
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
use std::thread;
use std::thread::sleep;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use houyicoder_core::snapshot::SnapshotStore;
use houyicoder_core::snapshot::UndoEntry;

/// Markers a child uses to coordinate with the parent (store built, may
/// proceed). Polled files keep the two processes off any shared clock.
const READY_MARKER: &str = "child.ready";
const RELEASE_MARKER: &str = "child.release";
/// The child records the snapshot directory it claimed so the parent can
/// compare it with its own.
const IDS_MARKER: &str = "child.ids";
/// Environment naming the child's role and its workspace root.
const ROLE_ENV: &str = "HOUYICODER_SNAPSHOT_TEST_ROLE";
const ROOT_ENV: &str = "HOUYICODER_SNAPSHOT_TEST_ROOT";
/// Generous deadline for a child to finish its side.
const CHILD_DEADLINE: Duration = Duration::from_secs(30);

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = env::temp_dir().join(format!("houyi-snapshot-{tag}-{}-{nanos}", id()));
    remove_dir_all(&root).ok();
    create_dir_all(&root).expect("create the workspace root");
    root
}

fn snapshot_name(entry: &UndoEntry) -> String {
    match entry {
        UndoEntry::CoWSnapshot { store_path, .. } => store_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
        UndoEntry::BeforeImage { .. } => panic!("a workspace snapshot is a copy-on-write entry"),
    }
}

/// Run the child half when the environment names one. Returns true in a
/// child, which then ends its test immediately: the child's stdout is not
/// the assertion point, the durable directory is.
fn run_child_role() -> bool {
    let Ok(role) = env::var(ROLE_ENV) else {
        return false;
    };
    if role != "snapshot" {
        panic!("unknown child role {role}");
    }
    let root = PathBuf::from(env::var(ROOT_ENV).expect("child workspace root"));
    let store = SnapshotStore::new(&root).expect("child store");
    write(root.join(READY_MARKER), b"").expect("signal ready");
    wait_for_file(&root.join(RELEASE_MARKER));
    let entry = store.snapshot(&[]).expect("child snapshot");
    write(root.join(IDS_MARKER), snapshot_name(&entry)).expect("record the snapshot name");
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

fn start_child(test_name: &str, root: &Path) -> Child {
    Command::new(current_exe().expect("the test binary path"))
        .arg("--exact")
        .arg(format!("concurrent_snapshots::{test_name}"))
        .arg("--nocapture")
        .env(ROLE_ENV, "snapshot")
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

/// Both processes seed their counter from the same on-disk scan, then take
/// one snapshot each. The second claim must take the next number instead of
/// writing into the directory the first process already owns.
#[test]
fn test_snapshots_keep_ids() {
    if run_child_role() {
        return;
    }
    let root = temp_root("two-stores");
    write(root.join("hello.txt"), "world").expect("write the workspace file");
    // Construct the parent store before the child starts, so both counters
    // seed from the same empty store.
    let store = SnapshotStore::new(&root).expect("parent store");
    let mut child = start_child("test_snapshots_keep_ids", &root);
    assert!(
        wait_for_file(&root.join(READY_MARKER)),
        "the child must build its store before the race starts"
    );
    write(root.join(RELEASE_MARKER), b"").expect("release the child");
    let parent = store.snapshot(&[]).expect("parent snapshot");
    wait_child(&mut child);
    let child_name = read_to_string(root.join(IDS_MARKER))
        .expect("the child records its snapshot name")
        .trim()
        .to_string();
    let parent_name = snapshot_name(&parent);
    assert_ne!(
        parent_name, child_name,
        "two processes must not claim one snapshot id"
    );
    let snaps = root.join(".houyicoder").join("snapshots");
    assert!(
        snaps.join(&parent_name).join("hello.txt").exists(),
        "the parent snapshot holds the workspace"
    );
    assert!(
        snaps.join(&child_name).join("hello.txt").exists(),
        "the child snapshot holds the workspace"
    );
    remove_dir_all(&root).ok();
}

/// Two stores probe the same root for reflink support at one moment. Each
/// probe owns the names of its two files, so neither can consume the other's
/// source or find its destination taken: both must report the same answer,
/// and neither may leave its probe files behind.
#[test]
fn test_reflink_probes_agree() {
    let root = temp_root("reflink-probe");
    let first = SnapshotStore::new(&root).expect("first store");
    let second = SnapshotStore::new(&root).expect("second store");
    let (a, b) = thread::scope(|scope| {
        let a = scope.spawn(|| first.can_reflink());
        let b = scope.spawn(|| second.can_reflink());
        (
            a.join().expect("the first probe"),
            b.join().expect("the second probe"),
        )
    });
    assert_eq!(a, b, "concurrent probes of one store root must agree");
    let snaps = root.join(".houyicoder").join("snapshots");
    let residue: Vec<String> = read_dir(&snaps)
        .expect("the probe creates the store root")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".reflink-test-"))
        .collect();
    assert!(
        residue.is_empty(),
        "each probe removes its own files: {residue:?}"
    );
    remove_dir_all(&root).ok();
}
