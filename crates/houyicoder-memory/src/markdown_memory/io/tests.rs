//! The atomic write's failure paths: a write that cannot create its temp
//! file, or cannot rename it over the target, reports the failure and leaves
//! no temp behind.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::read;
use std::fs::read_dir;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use houyicoder_context::MemoryError;

use super::relocate_record;
use super::write_bytes_atomic;

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = temp_dir().join(format!("markdown_io_test_{seq}_{}_{nanos}", id()));
    create_dir_all(&root).expect("create temp root");
    root
}

/// A write whose parent path is a regular file cannot create its temp file;
/// it reports the failure and leaves the directory untouched.
#[test]
fn test_write_bad_parent() {
    let root = temp_root();
    let blocker = root.join("blocker");
    write(&blocker, b"not a directory").expect("plant a file where a directory is needed");
    assert!(
        matches!(
            write_bytes_atomic(&blocker.join("topic.md"), b"data"),
            Err(MemoryError::Io)
        ),
        "a write whose parent is a file must report the failure"
    );
    assert!(blocker.is_file(), "the write must not disturb the blocker");
    assert_eq!(
        read_dir(&root).expect("root lists").count(),
        1,
        "a failed write leaves no temp behind"
    );
    remove_dir_all(&root).ok();
}

/// A rename that cannot replace its target removes the temp it wrote. The
/// target is a non-empty directory, which a rename over it cannot replace,
/// so the failure lands after the temp exists and cleanup must still run.
#[test]
fn test_write_cleans_temp() {
    let root = temp_root();
    let target = root.join("topic.md");
    create_dir_all(&target).expect("plant a directory at the target");
    write(target.join("occupied"), b"x").expect("make the target non-empty");
    assert!(
        matches!(write_bytes_atomic(&target, b"data"), Err(MemoryError::Io)),
        "a rename over a non-empty directory must report the failure"
    );
    let names: Vec<String> = read_dir(&root)
        .expect("root lists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["topic.md".to_string()],
        "a failed rename leaves no temp: {names:?}"
    );
    remove_dir_all(&root).ok();
}

/// A record moved to another root lands at the destination with its bytes and
/// leaves the source gone.
#[test]
fn test_relocate_moves_record() {
    let root = temp_root();
    let src = root.join("auto").join("alpha.md");
    create_dir_all(root.join("auto")).expect("create the source root");
    write(&src, b"body").expect("seed the record");
    let dst = root.join("project").join("alpha.md");
    create_dir_all(root.join("project")).expect("create the destination root");
    relocate_record(&src, &dst).expect("move the record");
    assert_eq!(read(&dst).expect("the destination reads"), b"body");
    assert!(!src.exists(), "the source must be gone after the move");
    remove_dir_all(&root).ok();
}

/// A move whose destination cannot be written reports the failure and leaves
/// the source in place, so a failed promote loses nothing.
#[test]
fn test_relocate_keeps_source() {
    let root = temp_root();
    let src = root.join("alpha.md");
    write(&src, b"body").expect("seed the record");
    let blocker = root.join("blocker");
    write(&blocker, b"not a directory").expect("plant a file where a directory is needed");
    let dst = blocker.join("alpha.md");
    assert!(
        matches!(relocate_record(&src, &dst), Err(MemoryError::Io)),
        "a move onto an unwritable destination must report the failure"
    );
    assert_eq!(
        read(&src).expect("the source reads"),
        b"body",
        "the source must survive a failed move"
    );
    remove_dir_all(&root).ok();
}
