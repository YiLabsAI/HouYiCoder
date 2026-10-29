//! Tests for reading the two sides of one recorded write as text: what each
//! side holds, and what stands in for a side that cannot be compared.

use std::fs::write;

use super::super::super::record::COMMITTED_FILE;
use super::super::super::test_support::{Scratch, operation_of, seed_write, session};
use super::super::RecoveryError;
use super::{SideText, TEXT_ROOM_BYTES};

/// The two sides of a write over a file that was there: the bytes it found
/// and the bytes it left.
#[tokio::test]
async fn test_text_holds_both_sides() {
    let scratch = Scratch::new("text-both");
    let session = session();
    let _stub = seed_write(&scratch, session).await;
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let text = scratch
        .transaction()
        .text(session, &operation)
        .await
        .expect("read the sides");

    assert_eq!(text.target, "a.txt");
    assert_eq!(
        text.before,
        SideText::Text {
            text: "old".to_string(),
            truncated: false,
        }
    );
    assert_eq!(
        text.after,
        SideText::Text {
            text: "new".to_string(),
            truncated: false,
        }
    );
}

/// A write that created its file has nothing on the earlier side, which the
/// detail names instead of showing an empty comparison.
#[tokio::test]
async fn test_text_names_created_file() {
    let scratch = Scratch::new("text-created");
    let session = session();
    let stub = scratch.stub();
    scratch
        .transaction()
        .write(Some(session), stub.as_ref(), "new.txt", b"hello".to_vec())
        .await
        .expect("write");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let text = scratch
        .transaction()
        .text(session, &operation)
        .await
        .expect("read the sides");

    assert_eq!(text.before, SideText::Absent);
    assert_eq!(
        text.after,
        SideText::Text {
            text: "hello".to_string(),
            truncated: false,
        }
    );
}

/// A file too large for the journal to keep is not compared: the side says
/// nothing was kept rather than claiming an empty file.
#[tokio::test]
async fn test_text_names_unkept_side() {
    let scratch = Scratch::new("text-unkept");
    let session = session();
    let stub = scratch.stub();
    write(scratch.file("a.txt"), "too long to keep").expect("seed the target");
    let transaction = scratch.transaction().with_backup_ceiling(2);
    transaction
        .write(Some(session), stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect("write");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let text = transaction
        .text(session, &operation)
        .await
        .expect("read the sides");

    assert_eq!(text.before, SideText::NotKept);
}

/// Bytes that are not text are named as such, so the detail does not show a
/// binary file as a diff of replacement characters.
#[tokio::test]
async fn test_text_names_binary_side() {
    let scratch = Scratch::new("text-binary");
    let session = session();
    let stub = scratch.stub();
    write(scratch.file("a.txt"), b"a\0b").expect("seed the target");
    scratch
        .transaction()
        .write(Some(session), stub.as_ref(), "a.txt", b"text".to_vec())
        .await
        .expect("write");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let text = scratch
        .transaction()
        .text(session, &operation)
        .await
        .expect("read the sides");

    assert_eq!(text.before, SideText::NotText);
}

/// A side larger than the room the detail has is cut there, and says so.
#[tokio::test]
async fn test_text_cuts_large_side() {
    let scratch = Scratch::new("text-large");
    let session = session();
    let stub = scratch.stub();
    let big = "a\n".repeat(TEXT_ROOM_BYTES / 2 + 1);
    write(scratch.file("a.txt"), &big).expect("seed the target");
    scratch
        .transaction()
        .write(Some(session), stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect("write");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let text = scratch
        .transaction()
        .text(session, &operation)
        .await
        .expect("read the sides");

    match text.before {
        SideText::Text {
            text: shown,
            truncated,
        } => {
            assert!(truncated, "the cut is stated");
            assert!(
                shown.len() <= TEXT_ROOM_BYTES,
                "the shown text stays in the room: {} bytes",
                shown.len()
            );
        }
        other => panic!("expected text, got {other:?}"),
    }
}

/// Many short lines are cut by the line room before the byte room is spent.
#[tokio::test]
async fn test_text_cuts_many_lines() {
    let scratch = Scratch::new("text-lines");
    let session = session();
    let stub = scratch.stub();
    write(scratch.file("a.txt"), "a\n".repeat(500)).expect("seed the target");
    scratch
        .transaction()
        .write(Some(session), stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect("write");
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    let text = scratch
        .transaction()
        .text(session, &operation)
        .await
        .expect("read the sides");

    match text.before {
        SideText::Text { truncated, .. } => assert!(truncated, "the cut is stated"),
        other => panic!("expected text, got {other:?}"),
    }
}

/// A name that is not a single path component is refused. The dot spelling
/// resolves through the join to the same record, so without the refusal a
/// caller could name a journal directory by a path it does not own.
#[tokio::test]
async fn test_text_refuses_escaping_name() {
    let scratch = Scratch::new("text-escape");
    let session = session();
    let _stub = seed_write(&scratch, session).await;
    let operation = operation_of(&scratch.sessions, COMMITTED_FILE);

    for name in [format!("./{operation}"), format!("../{operation}")] {
        let refused = scratch
            .transaction()
            .text(session, &name)
            .await
            .expect_err("a name that is not one component is refused");
        assert!(
            matches!(refused, RecoveryError::Unknown { .. }),
            "expected Unknown for {name}, got {refused:?}"
        );
    }

    // The record the refused spellings pointed at is still readable by its
    // own name, so the refusal is about the spelling and not a missing write.
    scratch
        .transaction()
        .text(session, &operation)
        .await
        .expect("the record itself is readable");
}

/// A name no record sits under is refused, so the detail says the row is gone
/// rather than showing two empty sides.
#[tokio::test]
async fn test_text_refuses_unknown() {
    let scratch = Scratch::new("text-unknown");
    let session = session();

    let refused = scratch
        .transaction()
        .text(session, "no-such-write")
        .await
        .expect_err("nothing is recorded under that name");

    assert!(
        matches!(refused, RecoveryError::Unknown { .. }),
        "expected Unknown, got {refused:?}"
    );
}
