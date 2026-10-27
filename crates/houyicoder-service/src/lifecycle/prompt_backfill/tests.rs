//! Tests for the bounded prompt scan.

use super::{ScanOutcome, backfill_session, scan_prompts};
use houyicoder_api::session::SessionLog;
use houyicoder_context::session_class::{LOG_FILE, session_dir};
use houyicoder_context::{
    EventId, PromptCoverage, SessionDescriptor, SessionDescriptorStore, SessionEvent, SessionId,
    SessionLogEntry, SessionProvenance,
};
use houyicoder_memory::{InMemoryDescriptorStore, LocalFileBackend};
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::io::Write;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = temp_dir().join(format!("prompt-backfill-{tag}-{}-{nanos}", id()));
    remove_dir_all(&root).ok();
    create_dir_all(&root).expect("mkdir root");
    root
}

fn entry(sid: SessionId, event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        event,
    }
}

async fn append(store: &SessionStore, sid: SessionId, event: SessionEvent) {
    store.append(entry(sid, event)).await.expect("append");
}

async fn append_input(store: &SessionStore, sid: SessionId, text: &str) {
    append(store, sid, SessionEvent::UserInput { text: text.into() }).await;
}

fn running() -> AtomicBool {
    AtomicBool::new(false)
}

/// A small log is read end to end: the head window reaches its end, so both
/// slots come from one pass and the record may claim complete coverage.
#[tokio::test]
async fn test_scan_reads_both_ends() {
    let root = temp_root("ends");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    append_input(&store, sid, "你好").await;
    append_input(&store, sid, "fix the login test").await;
    append_input(&store, sid, "rework the retry budget").await;
    let log: &dyn SessionLog = &store;
    let outcome = scan_prompts(log, sid, &running());
    let ScanOutcome::Scanned(scan) = outcome else {
        panic!("a readable log is scanned: {outcome:?}");
    };
    assert_eq!(
        scan.first.as_ref().map(|p| p.text.as_str()),
        Some("fix the login test"),
        "the greeting states no task, so the next input opens the subject"
    );
    assert_eq!(
        scan.last.as_ref().map(|p| p.text.as_str()),
        Some("rework the retry budget")
    );
    assert_eq!(scan.coverage, PromptCoverage::Complete);
    assert_eq!(
        scan.bytes_read,
        log.backend().log_size(sid),
        "the whole log falls inside one window"
    );
    remove_dir_all(&root).ok();
}

/// The same event shape carries a message another agent sent. Without a queue
/// identity it states that agent's words, not the user's, so neither slot
/// takes it.
#[tokio::test]
async fn test_scan_skips_agent_message() {
    let root = temp_root("agent-message");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    append_input(&store, sid, "你好").await;
    append(
        &store,
        sid,
        SessionEvent::MidTurnInput {
            text: "child finished: tests pass".into(),
            pending_input_id: None,
        },
    )
    .await;
    let log: &dyn SessionLog = &store;
    let ScanOutcome::Scanned(scan) = scan_prompts(log, sid, &running()) else {
        panic!("the log is readable");
    };
    assert!(scan.first.is_none(), "another agent's words state no task");
    assert!(scan.last.is_none());
    remove_dir_all(&root).ok();
}

/// A log longer than the two windows is reported as partial: a prompt between
/// them looks absent when it was merely not read.
#[tokio::test]
async fn test_scan_reports_partial() {
    let root = temp_root("partial");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    append_input(&store, sid, "opening task").await;
    // Each filler is a record well over a kilobyte and states no task, so the
    // tail window holds none of the prompt while the log grows past both
    // windows.
    let filler = format!("ok{}", " ".repeat(4096));
    for _ in 0..40 {
        append_input(&store, sid, &filler).await;
    }
    append_input(&store, sid, "rework the retry budget").await;
    let log: &dyn SessionLog = &store;
    let ScanOutcome::Scanned(scan) = scan_prompts(log, sid, &running()) else {
        panic!("the log is readable");
    };
    assert_eq!(
        scan.first.as_ref().map(|p| p.text.as_str()),
        Some("opening task")
    );
    assert_eq!(
        scan.last.as_ref().map(|p| p.text.as_str()),
        Some("rework the retry budget"),
        "the tail window holds the newest statement"
    );
    assert_eq!(scan.coverage, PromptCoverage::Partial);
    assert!(
        scan.bytes_read < log.backend().log_size(sid),
        "the scan reports the bytes it read, not the log's length"
    );
    remove_dir_all(&root).ok();
}

/// A statement that only the tail window reached is the newest one the scan
/// read, not the session's opening statement: the record gains a last prompt
/// and keeps an empty first slot.
#[tokio::test]
async fn test_scan_tail_keeps_first() {
    let root = temp_root("tail-only");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    // Each record is over a kilobyte and states nothing, so the head window
    // holds no task while the log grows past both windows.
    let filler = " ".repeat(4096);
    for _ in 0..40 {
        append_input(&store, sid, &filler).await;
    }
    append_input(&store, sid, "rework the retry budget").await;
    let descriptors = InMemoryDescriptorStore::new();
    descriptors
        .write_descriptor(sid, &descriptor())
        .expect("write record");
    let log: &dyn SessionLog = &store;
    let outcome = backfill_session(log, &descriptors, sid, &running()).expect("scan");
    let ScanOutcome::Scanned(scan) = outcome else {
        panic!("the log is readable");
    };
    assert!(scan.first.is_none(), "the head window saw no statement");
    assert_eq!(
        scan.last.as_ref().map(|p| p.text.as_str()),
        Some("rework the retry budget")
    );
    assert_eq!(
        scan.coverage,
        PromptCoverage::Partial,
        "the windows do not meet, so the scan cannot claim the log end to end"
    );
    let record = descriptors.read_descriptor(sid).expect("record");
    assert!(
        record.first_prompt.is_none(),
        "a statement outside the head window is never the first one"
    );
    assert_eq!(
        record.last_prompt.as_ref().map(|p| p.text.as_str()),
        Some("rework the retry budget")
    );
    remove_dir_all(&root).ok();
}

/// A record the windows reached but could not parse leaves the log
/// unaccounted for, so the scan reports partial coverage and the statements
/// it did read still count.
#[tokio::test]
async fn test_scan_reports_bad_record() {
    let root = temp_root("bad-record");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    append_input(&store, sid, "fix the login test").await;
    let dir = session_dir(&root, sid);
    let mut file = OpenOptions::new()
        .append(true)
        .open(dir.join(LOG_FILE))
        .expect("open the log");
    file.write_all(b"{not json}\n").expect("write the record");
    drop(file);
    let log: &dyn SessionLog = &store;
    let ScanOutcome::Scanned(scan) = scan_prompts(log, sid, &running()) else {
        panic!("the log is readable");
    };
    assert_eq!(
        scan.first.as_ref().map(|p| p.text.as_str()),
        Some("fix the login test")
    );
    assert_eq!(
        scan.coverage,
        PromptCoverage::Partial,
        "a record that does not parse is a gap in the account"
    );
    remove_dir_all(&root).ok();
}

#[tokio::test]
async fn test_scan_without_log() {
    let root = temp_root("absent");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let log: &dyn SessionLog = &store;
    assert_eq!(
        scan_prompts(log, SessionId::new(), &running()),
        ScanOutcome::Unsupported,
        "a session with no bytes behind it cannot be claimed either way"
    );
    remove_dir_all(&root).ok();
}

#[tokio::test]
async fn test_scan_stops_when_cancelled() {
    let root = temp_root("cancelled");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    append_input(&store, sid, "fix the login test").await;
    let log: &dyn SessionLog = &store;
    assert_eq!(
        scan_prompts(log, sid, &AtomicBool::new(true)),
        ScanOutcome::Cancelled
    );
    remove_dir_all(&root).ok();
}

fn descriptor() -> SessionDescriptor {
    SessionDescriptor::delegated_child(SessionProvenance::Fresh, 0, PromptCoverage::Unknown)
}

/// A record written before prompts were recorded gains both slots, and the
/// scan's coverage lands with it.
#[tokio::test]
async fn test_backfill_writes_record() {
    let root = temp_root("write");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    append_input(&store, sid, "fix the login test").await;
    append_input(&store, sid, "rework the retry budget").await;
    let descriptors = InMemoryDescriptorStore::new();
    descriptors
        .write_descriptor(sid, &descriptor())
        .expect("write record");
    let log: &dyn SessionLog = &store;
    let outcome = backfill_session(log, &descriptors, sid, &running()).expect("scan");
    assert!(matches!(outcome, ScanOutcome::Scanned(_)));
    let record = descriptors.read_descriptor(sid).expect("record");
    assert_eq!(
        record.first_prompt.as_ref().map(|p| p.text.as_str()),
        Some("fix the login test")
    );
    assert_eq!(
        record.last_prompt.as_ref().map(|p| p.text.as_str()),
        Some("rework the retry budget")
    );
    assert_eq!(record.prompt_coverage, PromptCoverage::Complete);
    remove_dir_all(&root).ok();
}

/// A scan the caller stopped writes nothing: the record keeps the account it
/// had rather than claiming a coverage the scan did not earn.
#[tokio::test]
async fn test_backfill_skips_cancelled() {
    let root = temp_root("skip");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    append_input(&store, sid, "fix the login test").await;
    let descriptors = InMemoryDescriptorStore::new();
    descriptors
        .write_descriptor(sid, &descriptor())
        .expect("write record");
    let log: &dyn SessionLog = &store;
    let outcome = backfill_session(log, &descriptors, sid, &AtomicBool::new(true)).expect("scan");
    assert_eq!(outcome, ScanOutcome::Cancelled);
    let record = descriptors.read_descriptor(sid).expect("record");
    assert!(record.first_prompt.is_none());
    assert_eq!(record.prompt_coverage, PromptCoverage::Unknown);
    remove_dir_all(&root).ok();
}
