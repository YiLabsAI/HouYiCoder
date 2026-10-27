use super::*;
use std::fs::read_dir;
#[cfg(unix)]
use std::fs::{Permissions, metadata};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::sync::atomic::{AtomicU64, Ordering};

use houyicoder_memory::InMemoryDescriptorStore;

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = temp_dir().join(format!("session-lock-{}-{n}", id()));
    create_dir_all(&p).unwrap();
    p
}

/// A first acquire succeeds and holds; a second acquire on the same sid
/// fails (held) while the first guard lives. Dropping the first frees the
/// lock so a third acquire succeeds.
#[test]
fn test_second_acquire_fails_recovers() {
    let root = temp_root();
    let sid = SessionId::new();
    let sid_str = sid.to_string();
    let first = SessionLock::acquire(&sid_str, &root).expect("first acquire");
    match SessionLock::acquire(&sid_str, &root) {
        Err(LockError::Held(_)) => {}
        Err(e) => panic!("second acquire must fail with Held, got: {e:?}"),
        Ok(_) => panic!("second acquire must fail while the first holds, but succeeded"),
    }
    drop(first);
    let _second = SessionLock::acquire(&sid_str, &root).expect("acquire after drop");
    remove_dir_all(&root).ok();
}

/// A session lock is owner-only, whether the acquire creates the file or
/// finds one. A mode that admits other users lets one of them take the lock
/// read-only and hold the session against its owner, and a file written by an
/// earlier build carries that mode until an open repairs it.
#[cfg(unix)]
#[test]
fn test_lock_file_owner_only() {
    let root = temp_root();
    let sid = SessionId::new();
    let sid_str = sid.to_string();
    let path = session_dir(&root, sid).join(LOCK_FILE);
    create_dir_all(path.parent().expect("the lock file has a directory"))
        .expect("mkdir session dir");
    let seeded_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o644)
        .open(&path)
        .expect("seed the file an earlier build could have left");
    // State the mode rather than inherit it from the process umask: one that
    // masks the group bits leaves the fixture owner-only, and the assertion
    // below would then fail without saying what it meant to set up.
    seeded_file
        .set_permissions(Permissions::from_mode(0o644))
        .expect("state the seeded mode");
    let seeded = metadata(&path).expect("seeded").permissions().mode();
    assert_eq!(
        seeded & 0o077,
        0o044,
        "the fixture must start wider than owner-only, got {seeded:o}"
    );
    let lock = SessionLock::acquire(&sid_str, &root).expect("acquire");
    let mode = metadata(&path)
        .expect("lock file exists")
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o077,
        0,
        "lock file mode {mode:o} admits other users"
    );
    drop(lock);
    let _r = remove_dir_all(&root);
}

/// The drop releases the kernel lock itself, so a second descriptor on the
/// same open file description -- what a child forked from a live host holds
/// until it executes -- does not keep the session held. Releasing by close
/// alone would hold it until the last copy closes.
#[test]
fn test_drop_releases_shared_handle() {
    let root = temp_root();
    let sid_str = SessionId::new().to_string();
    let lock = SessionLock::acquire(&sid_str, &root).expect("acquire");
    let shared = lock._file.file.try_clone().expect("share the description");
    drop(lock);
    let again =
        SessionLock::acquire(&sid_str, &root).expect("reacquire while the copy is still open");
    drop(again);
    drop(shared);
    remove_dir_all(&root).ok();
}

/// The registry answers for the locks this process holds: acquiring makes the
/// session read as its own, and the guard's drop withdraws it.
#[test]
fn test_lock_registry_tracks_guard() {
    let root = temp_root();
    let sid_str = SessionId::new().to_string();
    assert!(
        !SessionLock::held_in_this_process(&sid_str, &root),
        "nothing held yet"
    );
    let lock = SessionLock::acquire(&sid_str, &root).expect("acquire");
    assert!(SessionLock::held_in_this_process(&sid_str, &root));
    drop(lock);
    assert!(
        !SessionLock::held_in_this_process(&sid_str, &root),
        "the drop withdraws the session"
    );
    remove_dir_all(&root).ok();
}

/// A guard names the store it serializes with, and only that store's writer
/// reads the session as its own. Another store over the same root writes under
/// a lock of its own, so a pass belonging to it must not treat the session as
/// safe to fill.
#[test]
fn test_holder_names_its_store() {
    let root = temp_root();
    let sid_str = SessionId::new().to_string();
    let ours = InMemoryDescriptorStore::new();
    let other = InMemoryDescriptorStore::new();
    let lock = SessionLock::acquire(&sid_str, &root).expect("acquire");
    assert!(
        !SessionLock::held_by(StoreIdentity::of(&ours), &sid_str, &root),
        "an unbound guard belongs to no store yet"
    );
    lock.bind(StoreIdentity::of(&ours));
    assert!(SessionLock::held_by(
        StoreIdentity::of(&ours),
        &sid_str,
        &root
    ));
    assert!(
        !SessionLock::held_by(StoreIdentity::of(&other), &sid_str, &root),
        "another store's writes still race this guard"
    );
    assert!(SessionLock::held_in_this_process(&sid_str, &root));
    drop(lock);
    assert!(!SessionLock::held_by(
        StoreIdentity::of(&ours),
        &sid_str,
        &root
    ));
    remove_dir_all(&root).ok();
}

/// A sid that parses as no session id is refused and leaves nothing on disk:
/// joining the string would let it name a directory outside the sessions root.
#[test]
fn test_unparsable_sid_refused() {
    let root = temp_root();
    let refused = SessionLock::acquire("../../escape", &root);
    assert!(
        matches!(refused, Err(LockError::InvalidId(_))),
        "a non-session id is refused"
    );
    assert_eq!(
        read_dir(&root).map(|entries| entries.count()).unwrap_or(0),
        0,
        "a refusal creates nothing"
    );
    remove_dir_all(&root).ok();
}

/// Different sids do not contend (each has its own lock file).
#[test]
fn test_different_sids_dont_contend() {
    let root = temp_root();
    let a = SessionLock::acquire(&SessionId::new().to_string(), &root).expect("acquire a");
    let _b = SessionLock::acquire(&SessionId::new().to_string(), &root).expect("acquire b");
    drop(a);
    remove_dir_all(&root).ok();
}

/// A session whose directory is named in the ULID spelling locks beside
/// its log: joining the sid string would make a second directory for one
/// session, holding a lock over files no reader opens.
#[test]
fn test_acquire_resolves_legacy_dir() {
    let root = temp_root();
    let sid = SessionId::new();
    let legacy = root.join(sid.ulid_name());
    create_dir_all(&legacy).unwrap();
    write(legacy.join("log.jsonl"), "[]").unwrap();

    let _lock = SessionLock::acquire(&sid.to_string(), &root).expect("acquire");
    assert!(
        legacy.join(LOCK_FILE).is_file(),
        "the lock lands beside the log"
    );
    assert!(
        !root.join(sid.to_string()).exists(),
        "one session does not get a second directory"
    );
    remove_dir_all(&root).ok();
}
