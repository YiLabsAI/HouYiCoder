//! Tests for reading recorded writes back: each state a target can be in, the
//! rewrite that settles a write interrupted before its commit, and the three
//! things a restore can do to a target.

use std::fs::{create_dir_all, read_to_string, remove_file, write};
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(unix)]
use std::fs::{Permissions, metadata, rename, set_permissions};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use houyicoder_context::session_class::{FILE_TRANSACTION_DIR, session_dir};
use houyicoder_context::{ContextError, SandboxError};
use houyicoder_memory::InMemoryBackend;
use serde_json::json;

use super::super::FileTransaction;
use super::super::record::{COMMITTED_FILE, PREPARED_FILE};
use super::super::test_support::{
    Scratch, Stub, edit_prepared, find_record, operation_of, record, seed_write, session, stored,
    transactions_of,
};
use super::{
    ImageSummary, RecoveryError, RecoveryReport, RestoreOutcome, UnreadableWrite, WriteState,
};

/// The one write a report could not judge, when every other record was fine.
fn only_unreadable(report: &RecoveryReport) -> &UnreadableWrite {
    assert_eq!(
        report.unreadable.len(),
        1,
        "exactly one write could not be judged"
    );
    assert!(report.writes.is_empty(), "nothing else was listed");
    &report.unreadable[0]
}

/// The one committed record of a scratch tree.
fn committed_path(scratch: &Scratch) -> PathBuf {
    let found = find_record(&scratch.sessions, COMMITTED_FILE);
    assert_eq!(found.len(), 1, "exactly one committed record");
    found[0].clone()
}

#[tokio::test]
async fn test_recover_reports_landed() {
    let scratch = Scratch::new("recover-landed");
    let stub = scratch.stub();
    let session = session();
    write(scratch.file("a.txt"), "old").expect("seed the target");
    let transaction = scratch.transaction();

    transaction
        .write(Some(session), stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect("write");

    let report = transaction
        .recover(session, stub.as_ref())
        .await
        .expect("recover");
    assert_eq!(report.writes.len(), 1, "one recorded write");
    let found = &report.writes[0];
    assert_eq!(found.target, "a.txt");
    assert_eq!(found.state, WriteState::Applied);
    assert!(found.committed, "the write was committed");
    assert!(
        found.written_ms.is_some(),
        "a committed write states when it ran"
    );
    let prepared = record(&scratch.sessions, PREPARED_FILE);
    let committed = record(&scratch.sessions, COMMITTED_FILE);
    assert_eq!(
        found.before,
        ImageSummary {
            present: true,
            hash: prepared["before"]["block_ref"].as_str().map(str::to_string),
            bytes: Some(3),
        },
        "the earlier side names the bytes the write found"
    );
    assert_eq!(
        found.after,
        ImageSummary {
            present: true,
            hash: committed["after"]["block_ref"].as_str().map(str::to_string),
            bytes: Some(3),
        },
        "the later side names the bytes the write left"
    );
}

#[tokio::test]
async fn test_recover_settles_landed() {
    let scratch = Scratch::new("recover-settles");
    let stub = scratch.stub();
    let session = session();
    write(scratch.file("a.txt"), "old").expect("seed the target");
    let transaction = scratch.transaction();
    transaction
        .write(Some(session), stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect("write");
    remove_file(committed_path(&scratch)).expect("drop the commit, as a crash before it would");

    let report = transaction
        .recover(session, stub.as_ref())
        .await
        .expect("recover");

    let found = &report.writes[0];
    assert!(report.unreadable.is_empty(), "every record was readable");
    assert_eq!(found.state, WriteState::Applied);
    assert!(found.committed, "the recovery published the commit");
    assert!(
        found.written_ms.is_some(),
        "the settled write states when it ran"
    );
    let committed = record(&scratch.sessions, COMMITTED_FILE);
    assert_eq!(committed["target"], "a.txt");
    assert_eq!(committed["after"]["bytes"], 3);
    assert_eq!(
        stored(&scratch.backend(), &committed["after"]).await,
        b"new",
        "the commit names the bytes the write left"
    );
}

#[tokio::test]
async fn test_recover_reports_missing() {
    let scratch = Scratch::new("recover-missing");
    let stub = scratch.stub();
    let session = session();
    write(scratch.file("a.txt"), "old").expect("seed the target");
    let transaction = scratch.transaction();
    transaction
        .write(Some(session), stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect("write");
    write(scratch.file("a.txt"), "old").expect("put the earlier bytes back by hand");

    let report = transaction
        .recover(session, stub.as_ref())
        .await
        .expect("recover");

    assert_eq!(report.writes[0].state, WriteState::NotApplied);
}

#[tokio::test]
async fn test_recover_reports_change() {
    let scratch = Scratch::new("recover-change");
    let stub = scratch.stub();
    let session = session();
    write(scratch.file("a.txt"), "old").expect("seed the target");
    let transaction = scratch.transaction();
    transaction
        .write(Some(session), stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect("write");
    write(scratch.file("a.txt"), "other").expect("a later writer");

    let report = transaction
        .recover(session, stub.as_ref())
        .await
        .expect("recover");

    assert_eq!(report.writes[0].state, WriteState::Conflict);
}

#[tokio::test]
async fn test_recover_without_store_empty() {
    let scratch = Scratch::new("recover-rootless");
    let transaction = FileTransaction::new(Arc::new(InMemoryBackend::new()));

    let report = transaction
        .recover(session(), scratch.stub().as_ref())
        .await
        .expect("recover");

    assert!(
        report.writes.is_empty(),
        "a store with no root records no write"
    );
}

#[tokio::test]
async fn test_recover_without_records_empty() {
    let scratch = Scratch::new("recover-empty");

    let report = scratch
        .transaction()
        .recover(session(), scratch.stub().as_ref())
        .await
        .expect("recover");

    assert!(report.writes.is_empty(), "a session with no write records");
}

#[tokio::test]
async fn test_recover_reports_list_failure() {
    let scratch = Scratch::new("recover-list");
    let session = session();
    let dir = session_dir(&scratch.sessions, session);
    create_dir_all(&dir).expect("session directory");
    write(dir.join(FILE_TRANSACTION_DIR), "not a directory").expect("block the record directory");

    let error = scratch
        .transaction()
        .recover(session, scratch.stub().as_ref())
        .await
        .expect_err("a record directory that cannot be listed");

    assert!(matches!(error, RecoveryError::List { .. }), "got {error}");
}

#[tokio::test]
async fn test_recover_skips_unstarted_write() {
    let scratch = Scratch::new("recover-unstarted");
    let session = session();
    let dir = transactions_of(&scratch.sessions, session);
    create_dir_all(dir.join("0001-1-000000")).expect("a write that never published a record");

    let report = scratch
        .transaction()
        .recover(session, scratch.stub().as_ref())
        .await
        .expect("recover");

    assert!(
        report.writes.is_empty() && report.unreadable.is_empty(),
        "a directory with no record is not a write"
    );
}

#[tokio::test]
async fn test_recover_skips_stray_file() {
    let scratch = Scratch::new("recover-stray");
    let session = session();
    let dir = transactions_of(&scratch.sessions, session);
    create_dir_all(&dir).expect("record directory");
    write(dir.join("stray.txt"), "left by something else").expect("a file among the writes");

    let report = scratch
        .transaction()
        .recover(session, scratch.stub().as_ref())
        .await
        .expect("recover");

    assert!(
        report.writes.is_empty() && report.unreadable.is_empty(),
        "a file where writes live is not one"
    );
}

#[tokio::test]
async fn test_recover_reports_foreign_schema() {
    let scratch = Scratch::new("recover-schema");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    edit_prepared(&scratch.sessions, |record| record["schema"] = json!(2));

    let report = scratch
        .transaction()
        .recover(session, stub.as_ref())
        .await
        .expect("recover returns what it could judge");

    let unreadable = only_unreadable(&report);
    let error = &unreadable.error;
    assert!(matches!(error, RecoveryError::Record { .. }), "got {error}");
    assert!(
        error.to_string().contains("schema 2"),
        "{error} names the version it found"
    );
    assert!(
        unreadable.operation.is_some(),
        "the listing names the write it could not judge"
    );
}

#[tokio::test]
async fn test_recover_reports_broken_record() {
    let scratch = Scratch::new("recover-broken");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    let path = find_record(&scratch.sessions, PREPARED_FILE)[0].clone();
    write(&path, "{").expect("a record that does not parse");

    let report = scratch
        .transaction()
        .recover(session, stub.as_ref())
        .await
        .expect("recover returns what it could judge");

    let error = &only_unreadable(&report).error;
    assert!(matches!(error, RecoveryError::Record { .. }), "got {error}");
    assert!(
        error.to_string().contains("parse"),
        "{error} says the record did not parse"
    );
}

#[tokio::test]
async fn test_recover_reports_unreadable_record() {
    let scratch = Scratch::new("recover-unreadable");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    let path = find_record(&scratch.sessions, PREPARED_FILE)[0].clone();
    remove_file(&path).expect("clear the record file");
    create_dir_all(&path).expect("put a directory where the record was");

    let report = scratch
        .transaction()
        .recover(session, stub.as_ref())
        .await
        .expect("recover returns what it could judge");

    let error = &only_unreadable(&report).error;
    assert!(matches!(error, RecoveryError::Record { .. }), "got {error}");
    assert!(
        error.to_string().contains("read the record"),
        "{error} says the record could not be read"
    );
}

#[tokio::test]
async fn test_recover_keeps_readable_writes() {
    let scratch = Scratch::new("recover-partial");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    scratch
        .transaction()
        .write(Some(session), stub.as_ref(), "b.txt", b"new".to_vec())
        .await
        .expect("write");
    let mut broken = 0;
    for path in find_record(&scratch.sessions, PREPARED_FILE) {
        let body = read_to_string(&path).expect("read the record");
        if body.contains("\"a.txt\"") {
            write(&path, "{").expect("a record that does not parse");
            broken += 1;
        }
    }
    assert_eq!(broken, 1, "one record names the write to break");

    let report = scratch
        .transaction()
        .recover(session, stub.as_ref())
        .await
        .expect("recover returns what it could judge");

    assert_eq!(report.writes.len(), 1, "the readable write is still listed");
    assert_eq!(report.writes[0].target, "b.txt");
    assert_eq!(report.writes[0].state, WriteState::Applied);
    assert_eq!(
        report.unreadable.len(),
        1,
        "the broken write is reported beside it, not in place of it"
    );
}

#[tokio::test]
async fn test_recover_reports_missing_image() {
    let scratch = Scratch::new("recover-image");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    remove_file(committed_path(&scratch)).expect("the applied side comes from the prepared record");
    edit_prepared(&scratch.sessions, |record| {
        record["expected_after"]["block_ref"] = json!("missing-block");
    });

    let report = scratch
        .transaction()
        .recover(session, stub.as_ref())
        .await
        .expect("recover returns what it could judge");

    let error = &only_unreadable(&report).error;
    assert!(matches!(error, RecoveryError::Image { .. }), "got {error}");
}

#[tokio::test]
async fn test_recover_reports_target_escape() {
    let scratch = Scratch::new("recover-escape");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    edit_prepared(&scratch.sessions, |record| {
        record["target"] = json!("../outside.txt");
    });

    let report = scratch
        .transaction()
        .recover(session, stub.as_ref())
        .await
        .expect("recover returns what it could judge");

    let error = &only_unreadable(&report).error;
    assert!(matches!(error, RecoveryError::Target { .. }), "got {error}");
}

#[tokio::test]
async fn test_recover_reports_removed_target() {
    let scratch = Scratch::new("recover-removed");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    remove_file(scratch.file("a.txt")).expect("a file the write replaced is now gone");

    let report = scratch
        .transaction()
        .recover(session, stub.as_ref())
        .await
        .expect("recover");

    assert_eq!(
        report.writes[0].state,
        WriteState::Conflict,
        "the write found a file, and no file is not that file"
    );
}

#[tokio::test]
async fn test_recover_reports_removed_created() {
    let scratch = Scratch::new("recover-created");
    let stub = scratch.stub();
    let session = session();
    let transaction = scratch.transaction();
    transaction
        .write(Some(session), stub.as_ref(), "fresh.txt", b"new".to_vec())
        .await
        .expect("write");
    remove_file(scratch.file("fresh.txt")).expect("a file the write created is now gone");

    let report = transaction
        .recover(session, stub.as_ref())
        .await
        .expect("recover");

    let found = &report.writes[0];
    assert_eq!(
        found.state,
        WriteState::NotApplied,
        "a write that created its file left nothing behind"
    );
    assert!(found.committed, "the write still has its commit");
    assert!(
        !found.before.present,
        "a write that created its file found nothing"
    );
}

#[tokio::test]
async fn test_recover_reports_unreadable_target() {
    let scratch = Scratch::new("recover-dir");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    remove_file(scratch.file("a.txt")).expect("clear the target");
    create_dir_all(scratch.file("a.txt")).expect("put a directory where the target was");

    let report = scratch
        .transaction()
        .recover(session, stub.as_ref())
        .await
        .expect("recover returns what it could judge");

    let error = &only_unreadable(&report).error;
    assert!(matches!(error, RecoveryError::Read { .. }), "got {error}");
}

#[tokio::test]
async fn test_restore_writes_earlier_image() {
    let scratch = Scratch::new("restore-before");
    let stub = scratch.stub();
    let session = session();
    write(scratch.file("a.txt"), "old").expect("seed the target");
    let transaction = scratch.transaction();
    transaction
        .write(Some(session), stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect("write");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let outcome = transaction
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect("restore");

    assert_eq!(outcome, RestoreOutcome::Restored);
    assert_eq!(read_to_string(scratch.file("a.txt")).expect("read"), "old");
}

#[tokio::test]
async fn test_restore_removes_created() {
    let scratch = Scratch::new("restore-created");
    let stub = scratch.stub();
    let session = session();
    let transaction = scratch.transaction();
    transaction
        .write(Some(session), stub.as_ref(), "fresh.txt", b"new".to_vec())
        .await
        .expect("write");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let outcome = transaction
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect("restore");

    assert_eq!(outcome, RestoreOutcome::Restored);
    assert!(
        !scratch.file("fresh.txt").exists(),
        "the created file is gone"
    );
}

#[tokio::test]
async fn test_restore_keeps_changed() {
    let scratch = Scratch::new("restore-changed");
    let stub = scratch.stub();
    let session = session();
    write(scratch.file("a.txt"), "old").expect("seed the target");
    let transaction = scratch.transaction();
    transaction
        .write(Some(session), stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect("write");
    write(scratch.file("a.txt"), "edited by hand").expect("a later writer");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let outcome = transaction
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect("restore");

    let RestoreOutcome::Kept { copy } = outcome else {
        panic!("a changed target is kept, got {outcome:?}");
    };
    let copy = scratch
        .workspace
        .join(copy.expect("the earlier bytes went to a copy"));
    assert_eq!(read_to_string(&copy).expect("read the copy"), "old");
    assert_eq!(
        read_to_string(scratch.file("a.txt")).expect("read"),
        "edited by hand",
        "the target a later writer owns is left alone"
    );
}

#[tokio::test]
async fn test_restore_leaves_unchanged() {
    let scratch = Scratch::new("restore-unchanged");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    write(scratch.file("a.txt"), "old").expect("put the earlier bytes back by hand");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let outcome = scratch
        .transaction()
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect("restore");

    assert_eq!(outcome, RestoreOutcome::Unchanged);
    assert_eq!(read_to_string(scratch.file("a.txt")).expect("read"), "old");
}

#[tokio::test]
async fn test_restore_keeps_created_change() {
    let scratch = Scratch::new("restore-created-change");
    let stub = scratch.stub();
    let session = session();
    let transaction = scratch.transaction();
    transaction
        .write(Some(session), stub.as_ref(), "fresh.txt", b"new".to_vec())
        .await
        .expect("write");
    write(scratch.file("fresh.txt"), "edited by hand").expect("a later writer");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let outcome = transaction
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect("restore");

    assert_eq!(
        outcome,
        RestoreOutcome::Kept { copy: None },
        "the write created the file, so there are no earlier bytes to keep"
    );
    assert_eq!(
        read_to_string(scratch.file("fresh.txt")).expect("read"),
        "edited by hand"
    );
}

#[tokio::test]
async fn test_restore_refuses_unbacked() {
    let scratch = Scratch::new("restore-unbacked");
    let stub = scratch.stub();
    let session = session();
    let old = "x".repeat(20);
    write(scratch.file("big.txt"), &old).expect("seed the target");
    let transaction = scratch.transaction().with_backup_ceiling(10);
    transaction
        .write(Some(session), stub.as_ref(), "big.txt", b"new".to_vec())
        .await
        .expect("write");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let outcome = transaction
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect("restore");

    assert_eq!(outcome, RestoreOutcome::Unbacked);
    assert_eq!(
        read_to_string(scratch.file("big.txt")).expect("read"),
        "new",
        "a target with no earlier bytes is not touched"
    );
}

#[tokio::test]
async fn test_restore_refuses_unverified() {
    let scratch = Scratch::new("restore-unverified");
    let stub = scratch.stub();
    let session = session();
    let old = "x".repeat(20);
    write(scratch.file("big.txt"), &old).expect("seed the target");
    let transaction = scratch.transaction().with_backup_ceiling(10);
    transaction
        .write(Some(session), stub.as_ref(), "big.txt", b"new".to_vec())
        .await
        .expect("write");
    write(scratch.file("big.txt"), &old).expect("put unkept bytes back by hand");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let outcome = transaction
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect("restore");

    assert_eq!(
        outcome,
        RestoreOutcome::Unbacked,
        "the earlier bytes were never kept, so they cannot be written back"
    );
    assert_eq!(read_to_string(scratch.file("big.txt")).expect("read"), old);
}

#[tokio::test]
async fn test_restore_refuses_unbacked_copy() {
    let scratch = Scratch::new("restore-unbacked-copy");
    let stub = scratch.stub();
    let session = session();
    write(scratch.file("big.txt"), "x".repeat(20)).expect("seed the target");
    let transaction = scratch.transaction().with_backup_ceiling(10);
    transaction
        .write(Some(session), stub.as_ref(), "big.txt", b"new".to_vec())
        .await
        .expect("write");
    write(scratch.file("big.txt"), "other").expect("a later writer");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let outcome = transaction
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect("restore");

    assert_eq!(outcome, RestoreOutcome::Unbacked);
    assert_eq!(
        read_to_string(scratch.file("big.txt")).expect("read"),
        "other",
        "a target a later writer owns is left alone"
    );
}

#[tokio::test]
async fn test_restore_reports_image_failure() {
    let scratch = Scratch::new("restore-image");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    edit_prepared(&scratch.sessions, |record| {
        record["before"]["block_ref"] = json!("missing-block");
    });
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let error = scratch
        .transaction()
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect_err("an image the store does not hold");

    assert!(matches!(error, RecoveryError::Image { .. }), "got {error}");
    assert!(
        error.to_string().contains("missing-block"),
        "{error} names the image it could not read"
    );
}

#[tokio::test]
async fn test_restore_reports_kept_image() {
    let scratch = Scratch::new("restore-kept-image");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    edit_prepared(&scratch.sessions, |record| {
        record["before"]["block_ref"] = json!("missing-block");
    });
    write(scratch.file("a.txt"), "other").expect("a later writer");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let error = scratch
        .transaction()
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect_err("an image the store does not hold");

    assert!(matches!(error, RecoveryError::Image { .. }), "got {error}");
}

#[tokio::test]
async fn test_restore_reports_write_failure() {
    let scratch = Scratch::new("restore-write-failure");
    let session = session();
    seed_write(&scratch, session).await;
    let refusing = Stub::refusing(&scratch.workspace, &scratch.sessions);
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let error = scratch
        .transaction()
        .restore(session, &refusing, &operation)
        .await
        .expect_err("the restore write fails");

    assert!(
        matches!(error, RecoveryError::Restore { .. }),
        "got {error}"
    );
    assert_eq!(
        read_to_string(scratch.file("a.txt")).expect("read"),
        "new",
        "a failed restore leaves the target as the write left it"
    );
}

#[tokio::test]
async fn test_restore_reports_copy_failure() {
    let scratch = Scratch::new("restore-copy-failure");
    let session = session();
    seed_write(&scratch, session).await;
    write(scratch.file("a.txt"), "other").expect("a later writer");
    let refusing = Stub::refusing(&scratch.workspace, &scratch.sessions);
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let error = scratch
        .transaction()
        .restore(session, &refusing, &operation)
        .await
        .expect_err("the copy cannot be written");

    assert!(matches!(error, RecoveryError::Restore { path, .. } if path.contains("recovery-")));
}

#[cfg(unix)]
#[tokio::test]
async fn test_restore_reports_remove_failure() {
    let scratch = Scratch::new("restore-remove-failure");
    let stub = scratch.stub();
    let session = session();
    let transaction = scratch.transaction();
    transaction
        .write(
            Some(session),
            stub.as_ref(),
            "sub/fresh.txt",
            b"new".to_vec(),
        )
        .await
        .expect("write");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);
    let parent = scratch.file("sub");
    set_permissions(&parent, Permissions::from_mode(0o500)).expect("a directory without writes");

    let error = transaction
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect_err("the created file cannot be removed");

    set_permissions(&parent, Permissions::from_mode(0o755)).expect("put the permissions back");
    assert!(matches!(error, RecoveryError::Remove { .. }), "got {error}");
}

#[cfg(unix)]
#[tokio::test]
async fn test_restore_keeps_mode_change() {
    let scratch = Scratch::new("restore-mode");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    let target = scratch.file("a.txt");
    set_permissions(&target, Permissions::from_mode(0o600))
        .expect("other permissions than the write left");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let outcome = scratch
        .transaction()
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect("restore");

    let RestoreOutcome::Kept { copy } = outcome else {
        panic!("a re-permissioned target is kept, got {outcome:?}");
    };
    let copy = scratch
        .workspace
        .join(copy.expect("the earlier bytes went to a copy"));
    assert_eq!(read_to_string(&copy).expect("read the copy"), "old");
    assert_eq!(
        read_to_string(&target).expect("read"),
        "new",
        "the same bytes under other permissions are not the file the write left"
    );
    let mode = metadata(&target).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the restore does not change permissions");
}

#[cfg(unix)]
#[tokio::test]
async fn test_restore_keeps_identity_change() {
    let scratch = Scratch::new("restore-identity");
    let session = session();
    let stub = seed_write(&scratch, session).await;
    let replacement = scratch.file("a.tmp");
    write(&replacement, "new").expect("the same bytes in a new file");
    rename(&replacement, scratch.file("a.txt")).expect("a different file at the target path");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let outcome = scratch
        .transaction()
        .restore(session, stub.as_ref(), &operation)
        .await
        .expect("restore");

    let RestoreOutcome::Kept { copy } = outcome else {
        panic!("a replaced target is kept, got {outcome:?}");
    };
    let copy = scratch
        .workspace
        .join(copy.expect("the earlier bytes went to a copy"));
    assert_eq!(read_to_string(&copy).expect("read the copy"), "old");
    assert_eq!(
        read_to_string(scratch.file("a.txt")).expect("read"),
        "new",
        "the same bytes in another file are not the file the write left"
    );
}

#[tokio::test]
async fn test_restore_without_store_refused() {
    let scratch = Scratch::new("restore-rootless");
    let transaction = FileTransaction::new(Arc::new(InMemoryBackend::new()));

    let error = transaction
        .restore(session(), scratch.stub().as_ref(), "anything")
        .await
        .expect_err("a store with no root records no write");

    assert!(
        matches!(error, RecoveryError::Unknown { .. }),
        "got {error}"
    );
}

#[tokio::test]
async fn test_restore_reports_unknown() {
    let scratch = Scratch::new("restore-unknown");

    let error = scratch
        .transaction()
        .restore(session(), scratch.stub().as_ref(), "missing")
        .await
        .expect_err("a write the session never recorded");

    assert!(
        matches!(error, RecoveryError::Unknown { .. }),
        "got {error}"
    );
    assert!(
        error.to_string().contains("missing"),
        "{error} names the write it could not find"
    );
}

#[test]
fn test_recovery_error_names_cause() {
    let cases = [
        (
            RecoveryError::List {
                path: PathBuf::from("d"),
                source: IoError::new(ErrorKind::NotFound, "gone"),
            },
            "list the recorded writes d",
        ),
        (
            RecoveryError::Record {
                path: PathBuf::from("prepared.json"),
                reason: "no space".into(),
            },
            "read the record prepared.json",
        ),
        (
            RecoveryError::Target {
                path: "a.txt".into(),
                error: SandboxError::PathTraversal("escapes".into()),
            },
            "resolve the target a.txt",
        ),
        (
            RecoveryError::Read {
                path: PathBuf::from("a.txt"),
                source: IoError::new(ErrorKind::PermissionDenied, "denied"),
            },
            "read the target a.txt",
        ),
        (
            RecoveryError::Image {
                hash: "abc".into(),
                error: ContextError::Io,
            },
            "read the stored image abc",
        ),
        (
            RecoveryError::Unknown {
                operation: "op".into(),
            },
            "no recorded write named op",
        ),
        (
            RecoveryError::Restore {
                path: "a.txt".into(),
                error: SandboxError::Io("full".into()),
            },
            "restore a.txt",
        ),
        (
            RecoveryError::Remove {
                path: PathBuf::from("a.txt"),
                source: IoError::new(ErrorKind::PermissionDenied, "denied"),
            },
            "remove a.txt",
        ),
    ];
    for (error, expected) in cases {
        assert!(
            error.to_string().starts_with(expected),
            "{error} names what failed"
        );
    }
}
