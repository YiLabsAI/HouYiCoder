use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = temp_dir().join(format!("session-lock-{}-{n}", id()));
    create_dir_all(&p).unwrap();
    p
}

/// A first acquire succeeds and holds; a second acquire on the same sid
/// fails (held by other) while the first guard lives. Dropping the first
/// frees the lock so a third acquire succeeds.
#[test]
fn test_second_acquire_fails_recovers() {
    let root = temp_root();
    let sid = SessionId::new();
    let sid_str = sid.to_string();
    let first = SessionLock::acquire(&sid_str, &root).expect("first acquire");
    match SessionLock::acquire(&sid_str, &root) {
        Err(LockError::HeldByOther(_)) => {}
        Err(e) => panic!("second acquire must fail with HeldByOther, got: {e:?}"),
        Ok(_) => panic!("second acquire must fail while the first holds, but succeeded"),
    }
    drop(first);
    let _second = SessionLock::acquire(&sid_str, &root).expect("acquire after drop");
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
        legacy.join("session.lock").is_file(),
        "the lock lands beside the log"
    );
    assert!(
        !root.join(sid.to_string()).exists(),
        "one session does not get a second directory"
    );
    remove_dir_all(&root).ok();
}
