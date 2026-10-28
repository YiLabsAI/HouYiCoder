//! Tests for the file write journal: both sides of a write are stored before
//! the target is touched, every refusal leaves the target alone, and a write
//! that lands without its committed record is not reported as a failure.

use std::fs::{create_dir_all, read_to_string, write};
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
#[cfg(unix)]
use std::{
    fs::{Permissions, set_permissions},
    os::unix::fs::PermissionsExt,
};

use houyicoder_context::session_class::FILE_TRANSACTION_DIR;
use houyicoder_context::{ContextError, SandboxError, SessionId};

use super::WriteRefusal;
use super::record::{COMMITTED_FILE, PREPARED_FILE};
use super::test_support::{Scratch, Stub, find_record, record, stored};

#[tokio::test]
async fn test_write_records_both_sides() {
    let scratch = Scratch::new("both-sides");
    let backend = scratch.backend();
    let stub = scratch.stub();
    write(scratch.file("a.txt"), "old").expect("seed the target");

    scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "a.txt",
            b"new".to_vec(),
        )
        .await
        .expect("write");

    assert_eq!(read_to_string(scratch.file("a.txt")).expect("read"), "new");
    let prepared = record(&scratch.sessions, PREPARED_FILE);
    assert_eq!(prepared["schema"], 1);
    assert_eq!(prepared["target"], "a.txt");
    assert_eq!(prepared["before"]["present"], true);
    assert_eq!(prepared["before"]["bytes"], 3);
    assert_eq!(stored(&backend, &prepared["before"]).await, b"old");
    assert_eq!(stored(&backend, &prepared["expected_after"]).await, b"new");

    let committed = record(&scratch.sessions, COMMITTED_FILE);
    assert_eq!(committed["target"], "a.txt");
    assert_eq!(committed["after"]["bytes"], 3);
    assert_eq!(stored(&backend, &committed["after"]).await, b"new");
}

#[tokio::test]
async fn test_prepared_record_precedes_write() {
    let scratch = Scratch::new("prepared-first");
    let stub = scratch.stub();
    write(scratch.file("a.txt"), "old").expect("seed the target");

    scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "a.txt",
            b"new".to_vec(),
        )
        .await
        .expect("write");

    assert!(stub.observed.wrote.load(Ordering::SeqCst), "the write ran");
    assert_eq!(
        stub.observed.prepared.load(Ordering::SeqCst),
        1,
        "the prepared record was on disk when the write ran"
    );
    assert_eq!(
        stub.observed.committed.load(Ordering::SeqCst),
        0,
        "the committed record follows the write"
    );
}

#[tokio::test]
async fn test_new_file_records_absent() {
    let scratch = Scratch::new("new-file");
    let stub = scratch.stub();

    scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "fresh.txt",
            b"hello".to_vec(),
        )
        .await
        .expect("write");

    let prepared = record(&scratch.sessions, PREPARED_FILE);
    assert_eq!(prepared["before"]["present"], false);
    assert!(
        prepared["before"].get("block_ref").is_none(),
        "no bytes recorded"
    );
    assert_eq!(
        read_to_string(scratch.file("fresh.txt")).expect("read"),
        "hello"
    );
}

#[tokio::test]
async fn test_oversized_before_kept_unbacked() {
    let scratch = Scratch::new("oversized");
    let stub = scratch.stub();
    let old = "x".repeat(20);
    write(scratch.file("big.txt"), &old).expect("seed the target");

    scratch
        .transaction()
        .with_backup_ceiling(10)
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "big.txt",
            b"new".to_vec(),
        )
        .await
        .expect("write");

    let before = &record(&scratch.sessions, PREPARED_FILE)["before"];
    assert_eq!(before["present"], true, "the file was there");
    assert_eq!(before["bytes"], 20);
    assert!(
        before.get("block_ref").is_none(),
        "the record states the previous bytes were not kept"
    );
    assert_eq!(
        read_to_string(scratch.file("big.txt")).expect("read"),
        "new"
    );
}

#[tokio::test]
async fn test_unreadable_target_refused() {
    let scratch = Scratch::new("unreadable");
    let stub = scratch.stub();
    create_dir_all(scratch.file("folder")).expect("a directory in the way");

    let refusal = scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "folder",
            b"new".to_vec(),
        )
        .await
        .expect_err("a directory cannot be backed up");

    assert!(
        matches!(refusal, WriteRefusal::Unreadable { .. }),
        "got {refusal}"
    );
    assert!(
        !stub.observed.wrote.load(Ordering::SeqCst),
        "the target was not touched"
    );
    assert!(
        find_record(&scratch.sessions, PREPARED_FILE).is_empty(),
        "a refused write leaves no record"
    );
}

#[tokio::test]
async fn test_escape_refused_before_record() {
    let scratch = Scratch::new("escape");
    let stub = scratch.stub();

    let refusal = scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "../outside.txt",
            b"new".to_vec(),
        )
        .await
        .expect_err("a path outside the workspace is refused");

    assert!(
        matches!(refusal, WriteRefusal::Target { .. }),
        "got {refusal}"
    );
    assert!(
        !stub.observed.wrote.load(Ordering::SeqCst),
        "the target was not touched"
    );
}

#[tokio::test]
async fn test_record_failure_refuses_write() {
    let scratch = Scratch::new("no-record");
    let stub = scratch.stub();
    write(scratch.file("a.txt"), "old").expect("seed the target");
    let session = SessionId::new();
    let blocked = scratch
        .sessions
        .join(session.to_string())
        .join(FILE_TRANSACTION_DIR);
    create_dir_all(blocked.parent().expect("session directory")).expect("session directory");
    write(&blocked, "not a directory").expect("block the record directory");

    let refusal = scratch
        .transaction()
        .write(Some(session), stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect_err("a write without a record is refused");

    assert!(
        matches!(refusal, WriteRefusal::Record { .. }),
        "got {refusal}"
    );
    assert!(
        !stub.observed.wrote.load(Ordering::SeqCst),
        "the target was not touched"
    );
    assert_eq!(read_to_string(scratch.file("a.txt")).expect("read"), "old");
}

#[tokio::test]
async fn test_write_failure_keeps_prepared() {
    let scratch = Scratch::new("write-failure");
    let stub = Arc::new(Stub::refusing(&scratch.workspace, &scratch.sessions));

    let refusal = scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "a.txt",
            b"new".to_vec(),
        )
        .await
        .expect_err("the write itself fails");

    assert!(
        matches!(refusal, WriteRefusal::Write { .. }),
        "got {refusal}"
    );
    assert_eq!(
        find_record(&scratch.sessions, PREPARED_FILE).len(),
        1,
        "the prepared record stays for a recovery to read"
    );
    assert!(
        find_record(&scratch.sessions, COMMITTED_FILE).is_empty(),
        "nothing was committed"
    );
}

#[tokio::test]
async fn test_no_session_passes_through() {
    let scratch = Scratch::new("no-session");
    let stub = scratch.stub();

    scratch
        .transaction()
        .write(None, stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect("write");

    assert_eq!(read_to_string(scratch.file("a.txt")).expect("read"), "new");
    assert!(
        find_record(&scratch.sessions, PREPARED_FILE).is_empty(),
        "there is no session to record under"
    );
}

#[tokio::test]
async fn test_writes_serialize_per_target() {
    let scratch = Scratch::new("serialize");
    let stub = Arc::new(Stub::yielding(&scratch.workspace, &scratch.sessions));
    write(scratch.file("a.txt"), "old").expect("seed the target");
    let transaction = Arc::new(scratch.transaction());
    let session = SessionId::new();
    let write_once = |content: &'static [u8]| {
        let transaction = transaction.clone();
        let stub = stub.clone();
        tokio::spawn(async move {
            transaction
                .write(Some(session), stub.as_ref(), "a.txt", content.to_vec())
                .await
        })
    };

    let first = write_once(b"one");
    let second = write_once(b"two");
    first.await.expect("first task").expect("first write");
    second.await.expect("second task").expect("second write");

    let backend = scratch.backend();
    let mut earlier = Vec::new();
    for path in find_record(&scratch.sessions, PREPARED_FILE) {
        let body = read_to_string(&path).expect("read the record");
        let prepared: serde_json::Value = serde_json::from_str(&body).expect("parse the record");
        earlier.push(stored(&backend, &prepared["before"]).await);
    }
    assert_eq!(earlier.len(), 2, "both writes recorded");
    let chained = earlier
        .iter()
        .filter(|bytes| bytes.as_slice() == b"one")
        .count();
    assert_eq!(
        chained, 1,
        "one write found the bytes the other left, not the ones both found"
    );
}

/// A target whose bytes cannot be read is refused before the write; a target
/// that is readable but not writable gets as far as the write and is refused
/// there. The second case is the one a permission diagnosis has to name, and
/// the bytes the write could not replace must still be the ones on disk.
#[cfg(unix)]
#[tokio::test]
async fn test_write_refuses_readonly_target() {
    let scratch = Scratch::new("readonly-target");
    let stub = scratch.stub();
    let target = scratch.file("a.txt");
    write(&target, "old").expect("seed the target");
    set_permissions(&target, Permissions::from_mode(0o444)).expect("a file without writes");

    let refusal = scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "a.txt",
            b"new".to_vec(),
        )
        .await
        .expect_err("a target without writes is refused");

    assert!(
        matches!(&refusal, WriteRefusal::Write { path, .. } if path == "a.txt"),
        "the refusal names the target: {refusal}"
    );
    assert_eq!(
        read_to_string(&target).expect("read"),
        "old",
        "the bytes the write could not replace are still there"
    );
    assert_eq!(
        find_record(&scratch.sessions, PREPARED_FILE).len(),
        1,
        "the prepared record stays for a recovery to read"
    );
    assert!(
        find_record(&scratch.sessions, COMMITTED_FILE).is_empty(),
        "nothing was committed"
    );
}

/// A new file in a directory without writes fails at the same place, and the
/// directory is left as it was: no file appears.
#[cfg(unix)]
#[tokio::test]
async fn test_write_refuses_unwritable_parent() {
    let scratch = Scratch::new("readonly-parent");
    let stub = scratch.stub();
    let parent = scratch.file("locked");
    create_dir_all(&parent).expect("the parent directory");
    set_permissions(&parent, Permissions::from_mode(0o555)).expect("a directory without writes");

    let refusal = scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "locked/new.txt",
            b"new".to_vec(),
        )
        .await
        .expect_err("a file cannot be created in a directory without writes");

    set_permissions(&parent, Permissions::from_mode(0o755)).expect("put the permissions back");
    assert!(
        matches!(&refusal, WriteRefusal::Write { path, .. } if path == "locked/new.txt"),
        "the refusal names the target: {refusal}"
    );
    assert!(
        !scratch.file("locked/new.txt").exists(),
        "nothing was created"
    );
}

#[test]
fn test_refusal_names_its_cause() {
    let cases = [
        (
            WriteRefusal::Target {
                path: "a.txt".into(),
                error: SandboxError::PathTraversal("escapes".into()),
            },
            "resolve the write target a.txt",
        ),
        (
            WriteRefusal::Unreadable {
                path: PathBuf::from("a.txt"),
                error: IoError::new(ErrorKind::PermissionDenied, "denied"),
            },
            "read the previous a.txt",
        ),
        (
            WriteRefusal::Backup {
                path: "a.txt".into(),
                error: ContextError::Io,
            },
            "keep the previous bytes of a.txt",
        ),
        (
            WriteRefusal::Record {
                path: PathBuf::from("prepared.json"),
                reason: "no space".into(),
            },
            "write the record prepared.json",
        ),
        (
            WriteRefusal::Write {
                path: "a.txt".into(),
                error: SandboxError::Io("full".into()),
            },
            "write a.txt",
        ),
    ];
    for (refusal, expected) in cases {
        assert!(
            refusal.to_string().starts_with(expected),
            "{refusal} names what failed"
        );
    }
}
