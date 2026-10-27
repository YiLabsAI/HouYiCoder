//! What the lifecycle entry point reports for each way a reclaim pass can
//! end.

use std::env::temp_dir;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use super::*;

/// A test that shares a store root with a test running beside it would see
/// the other's sources and its held lock, so each one names its own root.
static ROOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A pass that frees everything its references do not name, so a test does
/// not have to wait out the write lease.
fn immediate() -> ReclaimPolicy {
    ReclaimPolicy::new(Duration::ZERO, Duration::from_secs(60))
}

fn temp_root() -> PathBuf {
    let sequence = ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let dir = temp_dir().join(format!("houyicoder_reclaim_entry_{}_{sequence}", id()));
    fs::create_dir_all(&dir).expect("create temp root");
    dir
}

/// A pass that reaches the end reports what it freed, so the maintenance
/// caller can log a reclaim rather than assume one.
#[test]
fn test_reclaim_removes_orphan() {
    let root = temp_root();
    let shard = root.join(".cas").join("ab");
    fs::create_dir_all(&shard).expect("create shard");
    let orphan = shard.join("ab00.bin.tmp");
    fs::write(&orphan, b"never renamed").expect("write orphan");
    let handle = OpenOptions::new()
        .write(true)
        .open(&orphan)
        .expect("open orphan");
    handle
        .set_modified(SystemTime::now() - Duration::from_secs(7200))
        .expect("age the orphan");

    let outcome = reclaim_store_blocks(&root, &immediate());
    let ReclaimOutcome::Completed(report) = outcome else {
        panic!("the pass should run: {outcome:?}");
    };
    assert_eq!(report.removed, 1, "the unreferenced block was freed");
    assert!(!orphan.exists(), "the freed block is gone");
    fs::remove_dir_all(&root).ok();
}

/// A pass that finds the store lock held did nothing, and says so instead of
/// reporting an empty reclaim as a finished one.
#[test]
fn test_reclaim_reports_busy() {
    let root = temp_root();
    let workspace = root.join(".cas").join(".gc");
    fs::create_dir_all(&workspace).expect("create reclaim workspace");
    let holder = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(workspace.join("lock"))
        .expect("open reclaim lock");
    holder.try_lock().expect("hold the reclaim lock");

    let outcome = reclaim_store_blocks(&root, &immediate());
    assert!(
        matches!(outcome, ReclaimOutcome::Skipped),
        "a held lock is a skipped pass: {outcome:?}"
    );
    drop(holder);
    fs::remove_dir_all(&root).ok();
}

/// A pass the budget stopped partway is reported as partial, not as a
/// finished reclaim with fewer blocks: the caller must be able to tell the
/// difference between "nothing left to free" and "not done yet".
#[test]
fn test_reclaim_reports_partial() {
    let root = temp_root();
    let shard = root.join(".cas").join("ab");
    fs::create_dir_all(&shard).expect("create shard");
    let orphan = shard.join("ab00.bin.tmp");
    fs::write(&orphan, b"waits for the next pass").expect("write orphan");
    let handle = OpenOptions::new()
        .write(true)
        .open(&orphan)
        .expect("open orphan");
    handle
        .set_modified(SystemTime::now() - Duration::from_secs(7200))
        .expect("age the orphan");

    let outcome = reclaim_store_blocks(&root, &ReclaimPolicy::new(Duration::ZERO, Duration::ZERO));
    let ReclaimOutcome::Partial(report) = outcome else {
        panic!("a spent sweep is a partial pass: {outcome:?}");
    };
    assert!(!report.complete);
    assert_eq!(report.removed, 1, "the shard it reached was finished");
    assert!(
        !orphan.exists(),
        "a partial pass keeps what its finished shard freed"
    );
    fs::remove_dir_all(&root).ok();
}

/// A source the pass cannot read stops it, and the reason names the source
/// so the operator can find what blocked the reclaim.
#[test]
fn test_reclaim_reports_unreadable() {
    let root = temp_root();
    let session = root.join("s1");
    fs::create_dir_all(&session).expect("create session dir");
    fs::write(session.join("log.jsonl"), "{\"id\":\"x\"}\nnot an event\n").expect("write log");

    let outcome = reclaim_store_blocks(&root, &immediate());
    let ReclaimOutcome::Stopped { error, report } = outcome else {
        panic!("an unreadable source stops the pass: {outcome:?}");
    };
    let reason = error.to_string();
    assert!(
        reason.contains("log.jsonl"),
        "the reason names the source: {reason}"
    );
    assert_eq!(report.removed, 0, "a stop before the sweep frees nothing");
    fs::remove_dir_all(&root).ok();
}
