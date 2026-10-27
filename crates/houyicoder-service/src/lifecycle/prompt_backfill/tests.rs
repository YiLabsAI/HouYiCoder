//! Tests for the bounded prompt scan.

use super::{ScanOutcome, backfill_session, scan_prompts};
use houyicoder_api::session::SessionLog;
use houyicoder_async::PFut;
use houyicoder_context::session_class::{LOG_FILE, session_dir};
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, EventId, LogRangeRead,
    PromptCoverage, ReverseRead, SessionDescriptor, SessionDescriptorStore, SessionEvent,
    SessionId, SessionLogEntry, SessionProvenance,
};
use houyicoder_memory::{InMemoryDescriptorStore, LocalFileBackend};
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::metadata;
use std::fs::remove_dir_all;
use std::fs::write;
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

/// A statement in the bytes between the two windows is not the newest one the
/// scan may report. The head's last hit sits above a stretch no read covered,
/// so a newer statement may live there: the record keeps the earliest slot and
/// gains no newest.
#[tokio::test]
async fn test_scan_withholds_gap_last() {
    let root = temp_root("gap");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    let filler = format!("ok{}", " ".repeat(4096));
    append_input(&store, sid, "opening task").await;
    for _ in 0..20 {
        append_input(&store, sid, &filler).await;
    }
    // Past the head window and still short of the tail window: no read covers
    // these bytes.
    append_input(&store, sid, "middle task").await;
    for _ in 0..20 {
        append_input(&store, sid, &filler).await;
    }
    let log: &dyn SessionLog = &store;
    let ScanOutcome::Scanned(scan) = scan_prompts(log, sid, &running()) else {
        panic!("the log is readable");
    };
    assert_eq!(
        scan.first.as_ref().map(|p| p.text.as_str()),
        Some("opening task")
    );
    assert!(
        scan.last.is_none(),
        "a newer statement may sit in the bytes no window read"
    );
    assert_eq!(scan.coverage, PromptCoverage::Partial);
    remove_dir_all(&root).ok();
}

/// The two windows meet: a log short enough that the tail window starts
/// inside the head window. A newest statement only the head reached then has
/// every byte after it read, by one window or the other, so it settles and the
/// record may claim complete coverage. Withholding it would leave the session
/// titled by its opening statement for good, since a complete record that
/// holds a first prompt is never rescanned.
#[tokio::test]
async fn test_scan_settles_met_windows() {
    let root = temp_root("met-windows");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    let log_path = session_dir(&root, sid).join(LOG_FILE);
    let filler = format!("ok{}", " ".repeat(4096));
    append_input(&store, sid, "opening task").await;
    for _ in 0..4 {
        append_input(&store, sid, &filler).await;
    }
    // Inside the head window, with the tail window starting above it: the log
    // runs on by more than one window, and by less than two.
    append_input(&store, sid, "middle task").await;
    while metadata(&log_path).map(|m| m.len()).unwrap_or(0) < 100 * 1024 {
        append_input(&store, sid, &filler).await;
    }
    let total = metadata(&log_path).expect("log exists").len();
    assert!(
        total < 118 * 1024,
        "fixture outgrew the range where the two windows meet: {total} bytes"
    );
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
        Some("middle task"),
        "the head and the tail read the log through, so its newest statement settles"
    );
    assert_eq!(scan.coverage, PromptCoverage::Complete);
    remove_dir_all(&root).ok();
}

/// The log's newest record is too wide for either window to return whole, so
/// the scan never sees what it states. The head's hit is then not the newest
/// statement, and reporting it would title the session by an older one.
#[tokio::test]
async fn test_scan_withholds_unread_newest() {
    let root = temp_root("wide-newest");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    append_input(&store, sid, "opening task").await;
    let wide = format!("fix the login test {}", " ".repeat(96 * 1024));
    append_input(&store, sid, &wide).await;
    let log: &dyn SessionLog = &store;
    let ScanOutcome::Scanned(scan) = scan_prompts(log, sid, &running()) else {
        panic!("the log is readable");
    };
    assert_eq!(
        scan.first.as_ref().map(|p| p.text.as_str()),
        Some("opening task")
    );
    assert!(
        scan.last.is_none(),
        "the widest record states text no read returned, so it may be newer"
    );
    assert_eq!(scan.coverage, PromptCoverage::Partial);
    remove_dir_all(&root).ok();
}

/// A line the scan cannot parse sits above every statement it read, so none of
/// its hits is the session's earliest: the unreadable line may state the first
/// task. The newest slot still fills, because that line is older than its hit.
#[tokio::test]
async fn test_first_withheld_after_bad() {
    let root = temp_root("bad-first");
    let sid = SessionId::new();
    let dir = session_dir(&root, sid);
    create_dir_all(&dir).expect("mkdir session");
    let valid = entry(
        sid,
        SessionEvent::UserInput {
            text: "fix the login test".into(),
        },
    );
    let body = format!("{{not json}}\n{}\n", serde_json::to_string(&valid).unwrap());
    write(dir.join(LOG_FILE), body).expect("write the log");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let log: &dyn SessionLog = &store;
    let ScanOutcome::Scanned(scan) = scan_prompts(log, sid, &running()) else {
        panic!("the log is readable");
    };
    assert!(
        scan.first.is_none(),
        "the unreadable line may state the session's first task"
    );
    assert_eq!(
        scan.last.as_ref().map(|p| p.text.as_str()),
        Some("fix the login test"),
        "the unreadable line is older than the hit, so the newest slot fills"
    );
    assert_eq!(scan.coverage, PromptCoverage::Partial);
    remove_dir_all(&root).ok();
}

/// Two lines the scan cannot parse straddle the only statement it read: the
/// one above may state the first task, the one below may state the newest, so
/// neither slot can be claimed. Reading the unreadable lines by their greatest
/// offset alone lets the first slot through on a window that may miss it.
#[tokio::test]
async fn test_first_withheld_around_bad() {
    let root = temp_root("bad-around");
    let sid = SessionId::new();
    let dir = session_dir(&root, sid);
    create_dir_all(&dir).expect("mkdir session");
    let valid = entry(
        sid,
        SessionEvent::UserInput {
            text: "fix the login test".into(),
        },
    );
    let body = format!(
        "{{not json}}\n{}\n{{also not json}}\n",
        serde_json::to_string(&valid).unwrap()
    );
    write(dir.join(LOG_FILE), body).expect("write the log");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let log: &dyn SessionLog = &store;
    let ScanOutcome::Scanned(scan) = scan_prompts(log, sid, &running()) else {
        panic!("the log is readable");
    };
    assert!(
        scan.first.is_none(),
        "a line above the hit may state the session's first task"
    );
    assert!(
        scan.last.is_none(),
        "a line below the hit may state the session's newest task"
    );
    assert_eq!(scan.coverage, PromptCoverage::Partial);
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
    assert!(
        scan.last.is_none(),
        "the unreadable record is newer than the hit, so what it states is unknown"
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

/// A tail window that cannot be read leaves the newest slot empty: in a log
/// longer than the head window its newest statement is not the head's newest,
/// and recording that one would title the session by an older statement.
#[tokio::test]
async fn test_tail_failure_omits_newest() {
    let root = temp_root("tail-failure");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    append_input(&store, sid, "opening task").await;
    let filler = format!("ok{}", " ".repeat(4096));
    for _ in 0..40 {
        append_input(&store, sid, &filler).await;
    }
    let unreadable = SessionStore::new(Box::new(TailFails {
        inner: LocalFileBackend::new(root.clone()),
    }));
    let log: &dyn SessionLog = &unreadable;
    let ScanOutcome::Scanned(scan) = scan_prompts(log, sid, &running()) else {
        panic!("the head is readable");
    };
    assert_eq!(
        scan.first.as_ref().map(|p| p.text.as_str()),
        Some("opening task")
    );
    assert!(
        scan.last.is_none(),
        "the head cannot speak for the newest end"
    );
    assert_eq!(scan.coverage, PromptCoverage::Partial);
    remove_dir_all(&root).ok();
}

/// A backend whose reverse read fails, so a scan has to decide what a
/// head-only read can claim. Every other method delegates to the real file
/// backend.
struct TailFails {
    inner: LocalFileBackend,
}

impl ContextBackend for TailFails {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        self.inner.append(event)
    }

    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.read_range(session, from, to)
    }

    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.replay(session)
    }

    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        self.inner.write_checkpoint(manifest)
    }

    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        self.inner.read_checkpoint(id)
    }

    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        self.inner.list_checkpoints(session)
    }

    fn read_log_range(&self, session: SessionId, byte_offset: u64, max_bytes: u64) -> LogRangeRead {
        self.inner.read_log_range(session, byte_offset, max_bytes)
    }

    fn read_lines_reverse(
        &self,
        _session: SessionId,
        _from_byte: u64,
        _max_bytes: u64,
    ) -> Result<ReverseRead, ContextError> {
        Err(ContextError::Io)
    }

    fn supports_log_windows(&self) -> bool {
        self.inner.supports_log_windows()
    }
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
