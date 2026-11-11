//! Two live processes writing one memory store must not lose either write.
//!
//! The store is shared by every process working in the same project, so its
//! write path is held to the same cross-process discipline as the rest of the
//! durable state: a lock on the root, per-writer temp names, atomic rename.
//! These tests run real child processes because an in-process imitation of a
//! file lock proves nothing about two processes contending for one store.

#![cfg(unix)]
#![expect(clippy::disallowed_methods, reason = "the test starts its own binary")]

use std::env;
use std::env::current_exe;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::metadata;
use std::fs::read_dir;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::process::abort;
use std::process::id;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::channel;
use std::thread;
use std::thread::sleep;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::MemoryEntry;
use houyicoder_context::MemoryError;
use houyicoder_context::MemorySource;
use houyicoder_memory::MarkdownMemoryProvider;

/// The root lock file every writer takes before touching the store.
const LOCK_NAME: &str = ".memory.lock";
/// Markers a child uses to coordinate with the parent (ready to race, may
/// proceed). Polled files keep the two processes off any shared clock.
const READY_MARKER: &str = "child.ready";
const RELEASE_MARKER: &str = "child.release";
/// Environment naming the child's role, its store root, and its key.
const ROLE_ENV: &str = "HOUYICODER_MEMORY_TEST_ROLE";
const ROOT_ENV: &str = "HOUYICODER_MEMORY_TEST_ROOT";
const KEY_ENV: &str = "HOUYICODER_MEMORY_TEST_KEY";
/// Writes per process in the interleaved scenario; enough rounds that an
/// unlocked read-modify-write of the index lets one side's lines win.
const WRITES_PER_PROCESS: usize = 40;
/// A blocked write is expected to stay blocked for the probe window; a
/// write that returns inside it did not wait on the lock.
const BLOCK_PROBE: Duration = Duration::from_millis(300);
/// Generous deadline for a child to finish its side.
const CHILD_DEADLINE: Duration = Duration::from_secs(30);

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = env::temp_dir().join(format!("houyi-memory-{tag}-{}-{nanos}", id()));
    remove_dir_all(&root).ok();
    create_dir_all(&root).expect("create the store root");
    root
}

fn entry(key: &str, body: &str) -> MemoryEntry {
    MemoryEntry::new(key, body, MemorySource::User)
}

/// Run the child half when the environment names one. Returns true in a
/// child, which then ends its test immediately: the child's stdout is not
/// the assertion surface, the durable store is.
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
                .expect("open the root lock");
            file.lock().expect("hold the root lock");
            write(root.join(READY_MARKER), b"").expect("signal ready");
            wait_for_file(&root.join(RELEASE_MARKER));
            drop(file);
        }
        "write" => {
            let key = env::var(KEY_ENV).expect("child key");
            write(root.join(READY_MARKER), b"").expect("signal ready");
            wait_for_file(&root.join(RELEASE_MARKER));
            let provider = MarkdownMemoryProvider::new(root.clone());
            for i in 0..WRITES_PER_PROCESS {
                provider
                    .add(entry(&key, &format!("{key} body {i}")))
                    .expect("child write");
            }
        }
        "crash" => {
            // Leave the state a writer leaves when it dies between the topic
            // change and the index publish, under the root lock it never
            // releases. The topic carries the index's own timestamp, so no
            // modification-time check can see the missing row.
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(root.join(LOCK_NAME))
                .expect("open the root lock");
            file.lock().expect("hold the root lock");
            let topic =
                "---\nname: alpha\ndescription: alpha body\nsource: user\n---\nalpha body\n";
            write(root.join("alpha.md"), topic).expect("land the topic");
            let index_time = metadata(root.join("MEMORY.md"))
                .expect("the index metadata")
                .modified()
                .expect("the index mtime");
            File::options()
                .write(true)
                .open(root.join("alpha.md"))
                .expect("open the landed topic")
                .set_modified(index_time)
                .expect("date the topic at the index");
            write(root.join(INDEX_DIRTY_NAME), b"").expect("leave the marker");
            write(root.join(READY_MARKER), b"").expect("signal ready");
            abort();
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

fn start_child(test_name: &str, role: &str, root: &Path, key: &str) -> Child {
    Command::new(current_exe().expect("the test binary path"))
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(ROLE_ENV, role)
        .env(ROOT_ENV, root)
        .env(KEY_ENV, key)
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

/// Wait for a child that is expected to die rather than finish. The exit
/// also closes its descriptors, so the root lock it held is free when this
/// returns.
fn wait_crashed_child(child: &mut Child) {
    let status = child.wait().expect("the child exits");
    assert!(
        !status.success(),
        "the crash role must die instead of exiting cleanly"
    );
}

/// A write either lands or reports the failure: it must wait for the root
/// lock rather than proceed under a lock another process holds. The child
/// holds the lock until told to release, so a write that returns inside the
/// probe window wrote without the lock.
#[test]
fn test_write_waits_for_lock() {
    if run_child_role() {
        return;
    }
    let root = temp_root("held-lock");
    let mut child = start_child("test_write_waits_for_lock", "hold-lock", &root, "alpha");
    assert!(
        wait_for_file(&root.join(READY_MARKER)),
        "the child must take the lock before the parent writes"
    );
    let provider = MarkdownMemoryProvider::new(root.clone());
    let (tx, rx) = channel();
    let writer = thread::spawn(move || {
        let outcome = provider.add(entry("alpha", "alpha fox body"));
        tx.send(outcome).expect("the writer reports its outcome");
    });
    assert!(
        matches!(rx.recv_timeout(BLOCK_PROBE), Err(RecvTimeoutError::Timeout)),
        "the write must wait while another process holds the root lock"
    );
    write(root.join(RELEASE_MARKER), b"").expect("release the child");
    let outcome = rx
        .recv_timeout(CHILD_DEADLINE)
        .expect("the write completes once the lock is free");
    assert!(outcome.is_ok(), "the write must land after the release");
    writer.join().expect("the writer thread ends");
    wait_child(&mut child);
    assert!(
        root.join("alpha.md").exists(),
        "the released write must leave its topic on disk"
    );
    remove_dir_all(&root).ok();
}

/// Both processes write different topics into one store, interleaved. Each
/// write is atomic and serialized on the root lock, so neither topic is
/// lost and the derived index names both.
#[test]
fn test_writers_keep_both() {
    if run_child_role() {
        return;
    }
    let root = temp_root("two-writers");
    let mut child = start_child("test_writers_keep_both", "write", &root, "beta");
    assert!(
        wait_for_file(&root.join(READY_MARKER)),
        "the child must be ready before the race starts"
    );
    let provider = MarkdownMemoryProvider::new(root.clone());
    write(root.join(RELEASE_MARKER), b"").expect("release the child");
    for i in 0..WRITES_PER_PROCESS {
        provider
            .add(entry("alpha", &format!("alpha body {i}")))
            .expect("parent write");
    }
    wait_child(&mut child);
    assert!(
        root.join("alpha.md").exists(),
        "the parent topic must exist"
    );
    assert!(root.join("beta.md").exists(), "the child topic must exist");
    let index = read_to_string(root.join("MEMORY.md")).expect("the index is readable");
    assert!(
        index.contains("- alpha ["),
        "the index must name the parent's key:\n{index}"
    );
    assert!(
        index.contains("- beta ["),
        "the index must name the child's key:\n{index}"
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

/// Assert one store operation waits while another process holds the root
/// lock, then completes once it is released. The operation is the mechanism
/// under test; the child is only the holder.
fn assert_waits_for_lock<F>(
    test_name: &str,
    tag: &str,
    provider: MarkdownMemoryProvider,
    root: &Path,
    op: F,
) where
    F: FnOnce(MarkdownMemoryProvider) -> Result<(), MemoryError> + Send + 'static,
{
    let mut child = start_child(test_name, "hold-lock", root, "alpha");
    assert!(
        wait_for_file(&root.join(READY_MARKER)),
        "the child must take the lock before the {tag} probe"
    );
    let (tx, rx) = channel();
    let writer = thread::spawn(move || {
        tx.send(op(provider))
            .expect("the writer reports its outcome");
    });
    assert!(
        matches!(rx.recv_timeout(BLOCK_PROBE), Err(RecvTimeoutError::Timeout)),
        "the {tag} must wait while another process holds the root lock"
    );
    write(root.join(RELEASE_MARKER), b"").expect("release the child");
    let outcome = rx
        .recv_timeout(CHILD_DEADLINE)
        .expect("the operation completes once the lock is free");
    assert!(outcome.is_ok(), "the {tag} must land after the release");
    writer.join().expect("the writer thread ends");
    wait_child(&mut child);
}

/// The marker a writer leaves when it dies before publishing the index. Its
/// name is part of the on-disk contract between runs, so this suite spells it
/// out rather than importing the writing process's private constant.
const INDEX_DIRTY_NAME: &str = ".memory.index-dirty";

/// A starting session checks and rebuilds under the same root lock a writer
/// takes. The marker forces the rebuild path, so a check that skips the lock
/// returns inside the probe instead of waiting for the child to release it.
#[test]
fn test_heal_waits_for_lock() {
    if run_child_role() {
        return;
    }
    let root = temp_root("heal-lock");
    MarkdownMemoryProvider::new(root.clone())
        .add(entry("alpha", "alpha body"))
        .expect("seed the topic");
    let marker = root.join(INDEX_DIRTY_NAME);
    write(&marker, b"").expect("leave the marker a dying writer leaves");
    let provider = MarkdownMemoryProvider::new(root.clone());
    assert_waits_for_lock("test_heal_waits_for_lock", "heal", provider, &root, |p| {
        p.rebuild_if_stale()
    });
    assert!(
        !marker.exists(),
        "the released heal rebuilds and clears the marker"
    );
    remove_dir_all(&root).ok();
}

/// A prune waits for the root lock rather than removing a topic while
/// another process rewrites the store.
#[test]
fn test_delete_waits_for_lock() {
    if run_child_role() {
        return;
    }
    let root = temp_root("delete-lock");
    MarkdownMemoryProvider::new(root.clone())
        .add(entry("alpha", "alpha body"))
        .expect("seed the topic");
    let provider = MarkdownMemoryProvider::new(root.clone());
    assert_waits_for_lock(
        "test_delete_waits_for_lock",
        "delete",
        provider,
        &root,
        |p| p.delete_memory("alpha"),
    );
    assert!(
        !root.join("alpha.md").exists(),
        "the release lets the prune land"
    );
    remove_dir_all(&root).ok();
}

/// Promotion moves a topic across roots and rewrites both indexes under
/// every root lock; it must wait for a holder in either root.
#[test]
fn test_promote_waits_for_lock() {
    if run_child_role() {
        return;
    }
    let root = temp_root("promote-lock");
    let user = root.join("user");
    let project = root.join("project");
    MarkdownMemoryProvider::new(root.clone())
        .add(entry("alpha", "alpha body"))
        .expect("seed the auto topic");
    let provider = MarkdownMemoryProvider::new_multi(vec![user, project.clone(), root.clone()]);
    assert_waits_for_lock(
        "test_promote_waits_for_lock",
        "promote",
        provider,
        &root,
        |p| p.promote_memory("alpha"),
    );
    assert!(
        project.join("alpha.md").exists(),
        "the release lets the promotion land"
    );
    remove_dir_all(&root).ok();
}

/// Demotion moves a topic back into the automatic scope under every root
/// lock; it must wait for a holder in either root.
#[test]
fn test_demote_waits_for_lock() {
    if run_child_role() {
        return;
    }
    let root = temp_root("demote-lock");
    let user = root.join("user");
    let project = root.join("project");
    MarkdownMemoryProvider::new(project.clone())
        .add(entry("alpha", "alpha body"))
        .expect("seed the project topic");
    let provider = MarkdownMemoryProvider::new_multi(vec![user, project, root.clone()]);
    assert_waits_for_lock(
        "test_demote_waits_for_lock",
        "demote",
        provider,
        &root,
        |p| p.demote_memory("alpha"),
    );
    assert!(
        root.join("alpha.md").exists(),
        "the release lets the demotion land"
    );
    remove_dir_all(&root).ok();
}

/// A derived-index rebuild rewrites every root, so it must wait for a holder
/// instead of writing an index another process is about to replace.
#[test]
fn test_rebuild_waits_for_lock() {
    if run_child_role() {
        return;
    }
    let root = temp_root("rebuild-lock");
    let provider = MarkdownMemoryProvider::new(root.clone());
    assert_waits_for_lock(
        "test_rebuild_waits_for_lock",
        "rebuild",
        provider,
        &root,
        |p| p.rebuild_index(),
    );
    assert!(
        root.join("MEMORY.md").exists(),
        "the release lets the rebuild land"
    );
    remove_dir_all(&root).ok();
}

/// A writer that dies holding the root lock and a marker leaves a store the
/// next process must repair: the kernel releases the lock when the process
/// dies, and the marker alone says the index is missing a topic whose
/// timestamp matches the index.
#[test]
fn test_crash_state_heals() {
    if run_child_role() {
        return;
    }
    let root = temp_root("crash-heal");
    MarkdownMemoryProvider::new(root.clone())
        .add(entry("seed", "seed body"))
        .expect("seed the store and its index");
    let mut child = start_child("test_crash_state_heals", "crash", &root, "alpha");
    assert!(
        wait_for_file(&root.join(READY_MARKER)),
        "the child must leave the crash state before it dies"
    );
    wait_crashed_child(&mut child);
    let provider = MarkdownMemoryProvider::new(root.clone());

    provider
        .rebuild_if_stale()
        .expect("a fresh process heals the dead writer's store");

    assert!(
        !root.join(INDEX_DIRTY_NAME).exists(),
        "the heal clears the marker the dead writer left"
    );
    let index = read_to_string(root.join("MEMORY.md")).expect("the index is readable");
    assert!(
        index.contains("- alpha ["),
        "the healed index names the topic the dead writer landed:\n{index}"
    );
    assert!(
        index.contains("- seed ["),
        "the heal keeps the row that was already published:\n{index}"
    );
    remove_dir_all(&root).ok();
}
