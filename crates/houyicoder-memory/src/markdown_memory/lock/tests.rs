//! The root lock's claim semantics on one root.

use std::env::temp_dir;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use super::RootLock;

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = temp_dir().join(format!("markdown_lock_test_{seq}_{}_{nanos}", id()));
    create_dir_all(&root).expect("create temp root");
    root
}

fn reopen(root: &Path) -> File {
    OpenOptions::new()
        .write(true)
        .open(root.join(".memory.lock"))
        .expect("open the lock file a second time")
}

#[test]
fn test_acquire_creates_lock_file() {
    let root = temp_root();
    let _lock = RootLock::acquire(&root).expect("claim a fresh root");
    assert!(
        root.join(".memory.lock").exists(),
        "claiming the root must leave its lock file"
    );
    remove_dir_all(&root).ok();
}

#[test]
fn test_drop_releases_lock() {
    let root = temp_root();
    let lock = RootLock::acquire(&root).expect("claim the root");
    let other = reopen(&root);
    assert!(
        matches!(other.try_lock(), Err(TryLockError::WouldBlock)),
        "a second handle must be refused while the claim is held"
    );
    drop(lock);
    other
        .try_lock()
        .expect("the lock frees when its claim drops");
    remove_dir_all(&root).ok();
}
