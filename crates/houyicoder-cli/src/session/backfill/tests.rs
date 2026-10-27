//! Tests for the background prompt backfill.

use super::{PassBounds, read_cursor, run_pass, write_cursor};
use houyicoder_async::PFut;
use houyicoder_context::session_class::{PageCursor, session_page};
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, EventId, LogRangeRead,
    PromptCoverage, PromptRecord, ReverseRead, SessionDescriptor, SessionDescriptorStore,
    SessionEvent, SessionId, SessionLogEntry, SessionProvenance,
};
use houyicoder_memory::{FileDescriptorStore, LocalFileBackend};
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = temp_dir().join(format!("prompt-backfill-cli-{tag}-{}-{nanos}", id()));
    remove_dir_all(&root).ok();
    create_dir_all(&root).expect("mkdir root");
    root
}

fn legacy() -> SessionDescriptor {
    SessionDescriptor::delegated_child(SessionProvenance::Fresh, 0, PromptCoverage::Unknown)
}

/// Build one session with a log holding the given input and a record written
/// by a version that recorded no prompt.
async fn seed_recordless(root: &Path, text: &str) -> SessionId {
    let sid = SessionId::new();
    let log = SessionStore::new(Box::new(LocalFileBackend::new(root.to_path_buf())));
    log.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput { text: text.into() },
    })
    .await
    .expect("append");
    FileDescriptorStore::new(root.to_path_buf())
        .write_descriptor(sid, &legacy())
        .expect("write record");
    sid
}

fn running() -> AtomicBool {
    AtomicBool::new(false)
}

/// A record that states nothing gains the task its log holds, and the pass
/// reports the session it scanned.
#[tokio::test]
async fn test_pass_fills_recordless() {
    let root = temp_root("fill");
    let sid = seed_recordless(&root, "fix the login test").await;
    let log = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let descriptors = FileDescriptorStore::new(root.clone());
    let report = run_pass(
        &root,
        &log,
        &descriptors,
        None,
        PassBounds::default(),
        &running(),
    );
    assert_eq!(report.examined, 1);
    assert_eq!(report.scanned, 1);
    assert!(report.bytes_read > 0);
    assert_eq!(report.next_cursor, None, "a short page ends the walk");
    let record = descriptors.read_descriptor(sid).expect("record");
    assert_eq!(
        record.last_prompt.as_ref().map(|p| p.text.as_str()),
        Some("fix the login test")
    );
    assert_eq!(record.prompt_coverage, PromptCoverage::Complete);
    remove_dir_all(&root).ok();
}

/// A record created for a session whose first prompt write then failed claims
/// whole-log coverage and states nothing, which is also what a session with no
/// task in it looks like. The pass re-reads it and fills the task its log
/// holds, because the two histories cannot be told apart by the record.
#[tokio::test]
async fn test_pass_fills_failed_write() {
    let root = temp_root("failed-write");
    let sid = seed_recordless(&root, "fix the login test").await;
    let descriptors = FileDescriptorStore::new(root.clone());
    descriptors
        .write_descriptor(
            sid,
            &SessionDescriptor::delegated_child(
                SessionProvenance::Fresh,
                0,
                PromptCoverage::Complete,
            ),
        )
        .expect("write record");
    let report = run_pass(
        &root,
        &SessionStore::new(Box::new(LocalFileBackend::new(root.clone()))),
        &descriptors,
        None,
        PassBounds::default(),
        &running(),
    );
    assert_eq!(report.scanned, 1);
    assert_eq!(
        descriptors
            .read_descriptor(sid)
            .expect("record")
            .last_prompt
            .map(|prompt| prompt.text),
        Some("fix the login test".to_string())
    );
    remove_dir_all(&root).ok();
}

/// A record that already carries a prompt is left alone: the live recorder
/// owns it, and the scan has nothing to add.
#[tokio::test]
async fn test_pass_skips_recorded() {
    let root = temp_root("recorded");
    let sid = seed_recordless(&root, "fix the login test").await;
    let descriptors = FileDescriptorStore::new(root.clone());
    let mut record = legacy();
    record.last_prompt = Some(PromptRecord {
        text: "recorded task".into(),
        event_id: EventId::new(),
    });
    record.prompt_coverage = PromptCoverage::Complete;
    descriptors
        .write_descriptor(sid, &record)
        .expect("write record");
    let report = run_pass(
        &root,
        &SessionStore::new(Box::new(LocalFileBackend::new(root.clone()))),
        &descriptors,
        None,
        PassBounds::default(),
        &running(),
    );
    assert_eq!(report.examined, 1);
    assert_eq!(report.scanned, 0);
    remove_dir_all(&root).ok();
}

/// A sub-agent's record is not the user's to list, so the pass does not spend
/// a window on it.
#[tokio::test]
async fn test_pass_skips_child() {
    let root = temp_root("child");
    let child = SessionDescriptor::delegated_child(
        SessionProvenance::SpawnedBy {
            parent_session_id: "parent".into(),
            subagent_type: "explore".into(),
            task_id: "task".into(),
        },
        0,
        PromptCoverage::Unknown,
    );
    let sid = SessionId::new();
    SessionStore::new(Box::new(LocalFileBackend::new(root.clone())))
        .append(SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "explore the parser".into(),
            },
        })
        .await
        .expect("append");
    let descriptors = FileDescriptorStore::new(root.clone());
    descriptors
        .write_descriptor(sid, &child)
        .expect("write record");
    let report = run_pass(
        &root,
        &SessionStore::new(Box::new(LocalFileBackend::new(root.clone()))),
        &descriptors,
        None,
        PassBounds::default(),
        &running(),
    );
    assert_eq!(report.scanned, 0);
    assert!(
        descriptors
            .read_descriptor(sid)
            .expect("record")
            .last_prompt
            .is_none()
    );
    remove_dir_all(&root).ok();
}

/// A store larger than one page migrates over several passes: the cursor
/// continues below the last session the page examined, and the pass that
/// reaches the oldest session restarts the walk.
#[tokio::test]
async fn test_pass_resumes_below_cursor() {
    let root = temp_root("resume");
    let mut sids = Vec::new();
    for text in ["first task", "second task", "third task"] {
        sids.push(seed_recordless(&root, text).await);
    }
    let log = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let descriptors = FileDescriptorStore::new(root.clone());
    let bounds = PassBounds {
        page: 2,
        ..PassBounds::default()
    };
    let page = session_page(&root, None, 2);
    let boundary = page.last().expect("a full page holds its sessions");
    let first = run_pass(&root, &log, &descriptors, None, bounds, &running());
    assert_eq!(first.examined, 2, "a full page stops at the bound");
    assert_eq!(
        first.next_cursor,
        Some(PageCursor {
            last_active: boundary.last_active,
            sid: boundary.sid,
        }),
        "the cursor continues below the last session examined"
    );
    let second = run_pass(
        &root,
        &log,
        &descriptors,
        first.next_cursor,
        bounds,
        &running(),
    );
    assert_eq!(second.examined, 1);
    assert_eq!(second.next_cursor, None, "the walk reached the oldest");
    assert!(
        sids.iter().all(|sid| descriptors
            .read_descriptor(*sid)
            .expect("record")
            .last_prompt
            .is_some()),
        "every session was filled across the two passes"
    );
    remove_dir_all(&root).ok();
}

/// A cancelled pass examines nothing and leaves the cursor where it found it,
/// so the next launch does the work.
#[tokio::test]
async fn test_pass_stops_when_cancelled() {
    let root = temp_root("cancelled");
    seed_recordless(&root, "fix the login test").await;
    let descriptors = FileDescriptorStore::new(root.clone());
    let report = run_pass(
        &root,
        &SessionStore::new(Box::new(LocalFileBackend::new(root.clone()))),
        &descriptors,
        None,
        PassBounds::default(),
        &AtomicBool::new(true),
    );
    assert_eq!(report.examined, 0);
    assert_eq!(report.next_cursor, None);
    remove_dir_all(&root).ok();
}

/// A pass whose cancel lands while one session's window is open stops above
/// that session, not below it, so the next pass scans it rather than leaving
/// it behind the cursor forever.
#[tokio::test]
async fn test_pass_retries_interrupted_session() {
    let root = temp_root("interrupted");
    for text in ["first task", "second task", "third task"] {
        seed_recordless(&root, text).await;
    }
    let order = session_page(&root, None, 10);
    let interrupted = order[1].sid;
    let cancelled = Arc::new(AtomicBool::new(false));
    let log = SessionStore::new(Box::new(StopOnRead {
        inner: LocalFileBackend::new(root.clone()),
        target: interrupted,
        cancelled: Arc::clone(&cancelled),
    }));
    let descriptors = FileDescriptorStore::new(root.clone());
    let bounds = PassBounds {
        page: 2,
        ..PassBounds::default()
    };
    let first = run_pass(&root, &log, &descriptors, None, bounds, &cancelled);
    assert_eq!(
        first.scanned, 1,
        "the session above the interruption was done"
    );
    assert_eq!(
        first.next_cursor,
        Some(PageCursor {
            last_active: order[0].last_active,
            sid: order[0].sid,
        }),
        "the cursor stands above the interrupted session"
    );
    assert!(
        descriptors
            .read_descriptor(interrupted)
            .expect("record")
            .last_prompt
            .is_none(),
        "the interrupted session was not written"
    );
    let second = run_pass(
        &root,
        &SessionStore::new(Box::new(LocalFileBackend::new(root.clone()))),
        &descriptors,
        first.next_cursor,
        bounds,
        &running(),
    );
    assert_eq!(
        second.examined, 2,
        "the next pass starts at the interruption"
    );
    assert!(
        descriptors
            .read_descriptor(interrupted)
            .expect("record")
            .last_prompt
            .is_some(),
        "the retried session is filled"
    );
    remove_dir_all(&root).ok();
}

/// A log whose next window read asks the pass to stop, so a cancellation
/// lands inside a scan. Only the reads a scan makes are forwarded: a default
/// for anything else fails loudly if the scan grows a new read.
struct StopOnRead {
    inner: LocalFileBackend,
    target: SessionId,
    cancelled: Arc<AtomicBool>,
}

impl ContextBackend for StopOnRead {
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
        if session == self.target {
            self.cancelled.store(true, Ordering::Relaxed);
        }
        self.inner.read_log_range(session, byte_offset, max_bytes)
    }

    fn read_lines_reverse(
        &self,
        session: SessionId,
        from_byte: u64,
        max_bytes: u64,
    ) -> Result<ReverseRead, ContextError> {
        self.inner.read_lines_reverse(session, from_byte, max_bytes)
    }

    fn supports_log_windows(&self) -> bool {
        self.inner.supports_log_windows()
    }
}

#[test]
fn test_cursor_round_trip() {
    let root = temp_root("cursor");
    assert!(read_cursor(&root).is_none(), "a missing cursor starts over");
    let page = PageCursor {
        last_active: 1_700_000_000,
        sid: SessionId::new(),
    };
    write_cursor(&root, Some(page));
    assert_eq!(read_cursor(&root), Some(page));
    write_cursor(&root, None);
    assert!(read_cursor(&root).is_none());
    remove_dir_all(&root).ok();
}
