//! Two live processes snapshotting one workspace must claim distinct ids.
//!
//! The snapshot store is shared by every process working in the workspace.
//! Each build fills a private staging tree and publishes it by renaming the
//! tree to a name it has claimed under the store namespace lock. These tests
//! run real child processes because the collision is two live processes
//! seeding their counters from the same on-disk scan.

#![cfg(unix)]
#![expect(clippy::disallowed_methods, reason = "the test starts its own binary")]

use std::env;
use std::env::current_exe;
use std::fs::OpenOptions;
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
use std::sync::mpsc::channel;
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
/// A builder child records that both snapshots are published, and later that
/// it restored the one it kept.
const DONE_MARKER: &str = "child.done";
const RESTORE_MARKER: &str = "child.restore";
const RESULT_MARKER: &str = "child.result";
/// Environment naming the child's role and its workspace root.
const ROLE_ENV: &str = "HOUYICODER_SNAPSHOT_TEST_ROLE";
const ROOT_ENV: &str = "HOUYICODER_SNAPSHOT_TEST_ROOT";
/// The directory the build barrier stops in, honored only by debug builds.
const BARRIER_ENV: &str = "HOUYICODER_SNAPSHOT_TEST_BARRIER";
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
    let root = PathBuf::from(env::var(ROOT_ENV).expect("child workspace root"));
    match role.as_str() {
        "snapshot" => run_snapshot_child(&root),
        "builder" => run_builder_child(&root),
        other => panic!("unknown child role {other}"),
    }
    true
}

fn run_snapshot_child(root: &Path) {
    let store = SnapshotStore::new(root).expect("child store");
    write(root.join(READY_MARKER), b"").expect("signal ready");
    wait_for_file(&root.join(RELEASE_MARKER));
    let entry = store.snapshot(&[]).expect("child snapshot");
    write(root.join(IDS_MARKER), snapshot_name(&entry)).expect("record the snapshot name");
}

/// Publish two snapshots, then hold the first one until the parent says it
/// may restore it. The second one is dropped, so only the first carries a
/// live lease when the parent prunes.
fn run_builder_child(root: &Path) {
    let store = SnapshotStore::new(root).expect("child store");
    write(root.join(READY_MARKER), b"").expect("signal ready");
    let held = store.snapshot(&[]).expect("first child snapshot");
    let dropped = store.snapshot(&[]).expect("second child snapshot");
    let names = format!("{}\n{}\n", snapshot_name(&held), snapshot_name(&dropped));
    write(root.join(IDS_MARKER), names).expect("record the snapshot names");
    drop(dropped);
    write(root.join(DONE_MARKER), b"").expect("signal done");
    assert!(
        wait_for_file(&root.join(RESTORE_MARKER)),
        "the parent releases the restore"
    );
    store.restore(&held).expect("child restore");
    write(root.join(RESULT_MARKER), b"").expect("signal restored");
}

fn child_snapshot_names(root: &Path) -> (String, String) {
    let record =
        read_to_string(root.join(IDS_MARKER)).expect("the child records its snapshot names");
    let mut names = record.lines().map(|line| line.trim().to_string());
    let held = names.next().expect("the held snapshot name");
    let dropped = names.next().expect("the unheld snapshot name");
    (held, dropped)
}

fn barrier_dir(root: &Path) -> PathBuf {
    root.join("barrier")
}

fn wait_stage(barrier: &Path, stage: &str) {
    assert!(
        wait_for_file(&barrier.join(stage)),
        "the build must reach its {stage} stage"
    );
}

fn release_stage(barrier: &Path, stage: &str) {
    write(barrier.join(format!("{stage}.release")), b"").expect("release the build stage");
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

fn start_child(test_name: &str, root: &Path, role: &str, barrier: Option<&Path>) -> Child {
    let mut command = Command::new(current_exe().expect("the test binary path"));
    command
        .arg("--exact")
        .arg(format!("concurrent_snapshots::{test_name}"))
        .arg("--nocapture")
        .env(ROLE_ENV, role)
        .env(ROOT_ENV, root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(barrier) = barrier {
        command.env(BARRIER_ENV, barrier);
    }
    command.spawn().expect("start the child process")
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
    let mut child = start_child("test_snapshots_keep_ids", &root, "snapshot", None);
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

fn staging_entries(snaps: &Path) -> Vec<String> {
    let Ok(entries) = read_dir(snaps) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".staging-"))
        .collect()
}

fn single_staging_dir(snaps: &Path) -> String {
    let mut dirs: Vec<String> = staging_entries(snaps)
        .into_iter()
        .filter(|name| !name.ends_with(".lease"))
        .collect();
    assert_eq!(
        dirs.len(),
        1,
        "the killed build leaves one staging directory"
    );
    dirs.pop().expect("the staging directory name")
}

fn age_staging_lease(snaps: &Path, staging: &str) {
    let lease = snaps.join(format!("{staging}.lease"));
    let file = OpenOptions::new()
        .write(true)
        .open(&lease)
        .expect("open the staging lease");
    let aged = SystemTime::now() - Duration::from_secs(7200);
    file.set_modified(aged).expect("age the staging lease");
}

/// From another process, a build stopped at one stage must be invisible to
/// re-link and untouched by prune, whatever it has copied so far.
fn assert_build_hidden(store: &SnapshotStore, barrier: &Path, stage: &str, why: &str) {
    wait_stage(barrier, stage);
    assert!(
        store.relink_undo_entries().is_empty(),
        "a build {why} is not restorable"
    );
    assert_eq!(store.prune(604800, 1, &[]), 0, "prune keeps a build {why}");
}

/// A build in progress is private: while the child copies the workspace, its
/// staging tree carries no snapshot name, so re-link cannot offer it and
/// prune cannot remove it. Once the child publishes, prune takes only the
/// snapshot the child no longer holds.
#[cfg(debug_assertions)]
#[test]
fn test_prune_keeps_active_build() {
    if run_child_role() {
        return;
    }
    let root = temp_root("active-build");
    for index in 0..200 {
        let path = root.join(format!("file-{index}.txt"));
        write(path, "payload").expect("write a workspace file");
    }
    write(root.join("sentinel.txt"), "keep-me").expect("write the sentinel");
    let barrier = barrier_dir(&root);
    create_dir_all(&barrier).expect("create the barrier directory");
    let snaps = root.join(".houyicoder").join("snapshots");
    let store = SnapshotStore::new(&root).expect("parent store");
    let mut child = start_child(
        "test_prune_keeps_active_build",
        &root,
        "builder",
        Some(&barrier),
    );
    assert!(
        wait_for_file(&root.join(READY_MARKER)),
        "the child must build its store before the race starts"
    );

    assert_build_hidden(&store, &barrier, "staged", "that published nothing");
    release_stage(&barrier, "staged");

    assert_build_hidden(&store, &barrier, "first-file", "that is partly copied");
    // A long build outlives the reclaim period, and the held lease is what
    // keeps prune off it anyway.
    let staging = single_staging_dir(&snaps);
    age_staging_lease(&snaps, &staging);
    assert_eq!(
        store.prune(604800, 1, &[]),
        0,
        "a live build older than the safe period is still kept"
    );
    assert!(
        !staging_entries(&snaps).is_empty(),
        "the live build survives prune past the safe period"
    );
    release_stage(&barrier, "first-file");

    assert_build_hidden(&store, &barrier, "copied", "that is fully copied");
    release_stage(&barrier, "copied");
    release_stage(&barrier, "publishing");

    assert!(
        wait_for_file(&root.join(DONE_MARKER)),
        "the child publishes both snapshots"
    );
    let (held_name, dropped_name) = child_snapshot_names(&root);
    assert!(
        snaps.join(&held_name).is_dir(),
        "the published snapshot exists under its final name"
    );
    assert!(
        snaps.join(format!(".{held_name}.complete")).is_file(),
        "a published snapshot carries the marker written before its rename"
    );
    write(root.join("sentinel.txt"), "changed").expect("mutate the sentinel");

    assert_eq!(
        store.prune(604800, 1, &[]),
        1,
        "prune removes only the snapshot no process holds"
    );
    assert!(
        snaps.join(&held_name).is_dir(),
        "a live process keeps its snapshot"
    );
    assert!(
        !snaps.join(&dropped_name).exists(),
        "the unheld snapshot is removed"
    );

    write(root.join(RESTORE_MARKER), b"").expect("release the restore");
    assert!(
        wait_for_file(&root.join(RESULT_MARKER)),
        "the child restores its snapshot"
    );
    wait_child(&mut child);
    assert_eq!(
        read_to_string(root.join("sentinel.txt")).expect("read the sentinel"),
        "keep-me",
        "the held snapshot restores the workspace"
    );
    remove_dir_all(&root).ok();
}

/// A build whose process died leaves a staging tree behind. The reclaim waits
/// for the safe period and for the lease to be free, then removes both the
/// tree and its sentinel; a fresh orphan is left alone.
#[cfg(debug_assertions)]
#[test]
fn test_prune_reclaims_dead_build() {
    if run_child_role() {
        return;
    }
    let root = temp_root("dead-build");
    for index in 0..20 {
        let path = root.join(format!("file-{index}.txt"));
        write(path, "payload").expect("write a workspace file");
    }
    let barrier = barrier_dir(&root);
    create_dir_all(&barrier).expect("create the barrier directory");
    let snaps = root.join(".houyicoder").join("snapshots");
    let store = SnapshotStore::new(&root).expect("parent store");
    let mut child = start_child(
        "test_prune_reclaims_dead_build",
        &root,
        "builder",
        Some(&barrier),
    );
    assert!(
        wait_for_file(&root.join(READY_MARKER)),
        "the child must build its store before the race starts"
    );
    wait_stage(&barrier, "staged");
    release_stage(&barrier, "staged");
    wait_stage(&barrier, "first-file");
    child.kill().expect("kill the builder");
    let _status = child.wait().expect("the killed builder exits");

    let staging = single_staging_dir(&snaps);
    assert_eq!(
        store.prune(604800, u64::MAX, &[]),
        0,
        "a fresh orphan is younger than the safe period"
    );
    assert!(
        !staging_entries(&snaps).is_empty(),
        "a fresh staging tree survives prune"
    );

    age_staging_lease(&snaps, &staging);
    assert_eq!(
        store.prune(604800, u64::MAX, &[]),
        0,
        "no snapshot was ever published"
    );
    assert!(
        staging_entries(&snaps).is_empty(),
        "the stale build and its sentinel are reclaimed"
    );
    remove_dir_all(&root).ok();
}

/// A publisher holds the store namespace lock while it claims the final name,
/// writes the complete marker, and renames its tree in. A prune that starts
/// at that instant must wait for the lock rather than fail or deadlock, and
/// the snapshot being published must survive it.
#[cfg(debug_assertions)]
#[test]
fn test_publish_blocks_prune() {
    if run_child_role() {
        return;
    }
    let root = temp_root("publish-prune");
    write(root.join("hello.txt"), "world").expect("write the workspace file");
    let barrier = barrier_dir(&root);
    create_dir_all(&barrier).expect("create the barrier directory");
    let snaps = root.join(".houyicoder").join("snapshots");
    let mut child = start_child(
        "test_publish_blocks_prune",
        &root,
        "builder",
        Some(&barrier),
    );
    assert!(
        wait_for_file(&root.join(READY_MARKER)),
        "the child must build its store before the race starts"
    );
    for stage in ["staged", "first-file", "copied"] {
        wait_stage(&barrier, stage);
        release_stage(&barrier, stage);
    }
    wait_stage(&barrier, "publishing");
    let store = SnapshotStore::new(&root).expect("pruner store");
    let (started_tx, started_rx) = channel();
    let pruning = thread::spawn(move || {
        started_tx.send(()).expect("signal the prune start");
        store.prune(604800, u64::MAX, &[])
    });
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the prune starts");
    sleep(Duration::from_millis(100));
    assert!(
        !pruning.is_finished(),
        "prune that started while the publisher holds the lock cannot finish"
    );
    release_stage(&barrier, "publishing");
    let removed = pruning.join().expect("the prune finishes");
    assert_eq!(removed, 0, "prune keeps the snapshot being published");
    assert!(
        wait_for_file(&root.join(DONE_MARKER)),
        "the child publishes both snapshots"
    );
    let (held_name, _) = child_snapshot_names(&root);
    assert!(
        snaps.join(&held_name).is_dir(),
        "the snapshot renamed in while prune waited exists"
    );
    assert!(
        snaps.join(format!(".{held_name}.complete")).is_file(),
        "the published snapshot carries its complete marker"
    );
    write(root.join(RESTORE_MARKER), b"").expect("release the restore");
    assert!(
        wait_for_file(&root.join(RESULT_MARKER)),
        "the child restores its snapshot"
    );
    wait_child(&mut child);
    assert_eq!(
        read_to_string(root.join("hello.txt")).expect("read the workspace file"),
        "world",
        "the published snapshot restores the workspace"
    );
    remove_dir_all(&root).ok();
}
