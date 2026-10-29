//! Tests for the durable capture: what it publishes, when it stops, and how a
//! publish failure is reported.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, EventId, SessionLogEntry,
};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::block_ref::block_addresses;
use houyicoder_session::SessionStore;
use serde_json::{Value, json};

fn store() -> Arc<dyn SessionLog> {
    Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())))
}

fn begin(log: &Arc<dyn SessionLog>, budget: Option<usize>) -> (SessionId, Arc<dyn ResultCapture>) {
    let session = SessionId::new();
    let source: Arc<dyn ResultCaptureSource> = match budget {
        Some(budget) => Arc::new(SessionResultCapture::with_budget(log.clone(), budget)),
        None => Arc::new(SessionResultCapture::new(log.clone())),
    };
    let capture = source
        .begin(session, "call-1", "bash")
        .expect("a capture source starts a capture");
    (session, capture)
}

fn read_block(log: &Arc<dyn SessionLog>, hash: &BlockHash) -> Vec<u8> {
    pollster::block_on(log.backend().block_get(hash)).expect("published block is readable")
}

/// A call that printed nothing gains no block: there is no raw output to
/// reach, so the record would name an empty capture.
#[test]
fn test_empty_output_keeps_nothing() {
    let log = store();
    let (_, capture) = begin(&log, None);
    let published =
        pollster::block_on(capture.finish(CallEnd::Exited(Some(0)))).expect("finish succeeds");
    assert!(
        published.is_none(),
        "nothing was read, so nothing is written"
    );
}

/// Output is published as one block per stream plus a record that binds them
/// to the call, so a reader can reach the bytes the bounded result dropped.
#[test]
fn test_large_output_publishes_streams() {
    let log = store();
    let (session, capture) = begin(&log, None);
    let stdout = vec![b'o'; 40 * 1024];
    let stderr = vec![b'e'; 33 * 1024];
    assert_eq!(
        capture.receive(CapturedStream::Stdout, &stdout),
        CaptureVerdict::Continue
    );
    assert_eq!(
        capture.receive(CapturedStream::Stderr, &stderr),
        CaptureVerdict::Continue
    );
    let published = pollster::block_on(capture.finish(CallEnd::Exited(Some(0))))
        .expect("finish succeeds")
        .expect("received output is published");

    assert_eq!(read_block(&log, published.stdout.as_ref().unwrap()), stdout);
    assert_eq!(read_block(&log, published.stderr.as_ref().unwrap()), stderr);
    assert_eq!(published.stdout_bytes, stdout.len() as u64);
    assert_eq!(published.stderr_bytes, stderr.len() as u64);
    assert!(published.stop.is_none(), "the command ended on its own");

    let record: Value =
        serde_json::from_slice(&read_block(&log, &published.record)).expect("record is JSON");
    assert_eq!(record["version"], CAPTURE_RECORD_VERSION);
    assert_eq!(record["session"], session.to_string());
    assert_eq!(record["call"], "call-1");
    assert_eq!(record["tool"], "bash");
    assert_eq!(record["exit"], 0);
    assert_eq!(record["completeness"], "complete");
    assert_eq!(record["stdout"]["bytes"], stdout.len() as u64);
    assert_eq!(record["stderr"]["bytes"], stderr.len() as u64);
    assert_eq!(
        record["stdout"]["block"]["block_ref"],
        published.stdout.as_ref().unwrap().0,
        "the record names the block that holds the bytes"
    );

    let addresses = block_addresses(&record);
    assert!(
        addresses.contains(&published.stdout.as_ref().unwrap().0.as_str()),
        "the record names the stdout block under the walker's key"
    );
    assert!(
        addresses.contains(&published.stderr.as_ref().unwrap().0.as_str()),
        "the record names the stderr block under the walker's key"
    );
}

/// A capture that reaches its ceiling keeps what fits, reports the range it
/// saved, and refuses every later chunk so the exec stops the producer.
#[test]
fn test_budget_stop_keeps_head() {
    let log = store();
    let (_, capture) = begin(&log, Some(1024));
    assert_eq!(
        capture.receive(CapturedStream::Stdout, &[b'a'; 512]),
        CaptureVerdict::Continue
    );
    let verdict = capture.receive(CapturedStream::Stdout, &[b'b'; 1024]);
    match verdict {
        CaptureVerdict::Stop(stop) => {
            assert_eq!(stop.reason, CaptureStopReason::ByteBudget);
            assert_eq!(stop.stdout_bytes, 1024, "the ceiling's worth is kept");
        }
        CaptureVerdict::Continue => panic!("a chunk over the ceiling stops the capture"),
    }
    assert!(
        matches!(
            capture.receive(CapturedStream::Stdout, b"more"),
            CaptureVerdict::Stop(_)
        ),
        "a stopped capture stays stopped instead of growing"
    );

    let published = pollster::block_on(capture.finish(CallEnd::Exited(None)))
        .expect("finish succeeds")
        .expect("a stopped capture publishes what it kept");
    let kept = read_block(&log, published.stdout.as_ref().unwrap());
    assert_eq!(kept.len(), 1024);
    assert_eq!(&kept[..512], &[b'a'; 512]);
    assert_eq!(&kept[512..], &[b'b'; 512]);
    let record: Value =
        serde_json::from_slice(&read_block(&log, &published.record)).expect("record is JSON");
    assert!(record["exit"].is_null(), "nothing exited");
    assert_eq!(record["completeness"], "truncated");
    assert_eq!(record["stop_reason"], json!("byte_budget"));
    assert!(
        record["created_ms"].is_u64() || record["created_ms"].is_null(),
        "a clock reading this host cannot take is absent, never zero: {}",
        record["created_ms"]
    );
}

/// One budget covers both streams. A chunk that overruns it on stderr keeps
/// the stderr prefix, stops the capture, and the record keeps the two stream
/// lengths apart instead of charging one stream's bytes to the other.
#[test]
fn test_stderr_shares_budget() {
    let log = store();
    let (_, capture) = begin(&log, Some(256));
    assert_eq!(
        capture.receive(CapturedStream::Stdout, &[b'a'; 128]),
        CaptureVerdict::Continue
    );
    let stop = match capture.receive(CapturedStream::Stderr, &[b'e'; 512]) {
        CaptureVerdict::Stop(stop) => stop,
        CaptureVerdict::Continue => panic!("the budget covers both streams"),
    };
    assert_eq!(stop.stdout_bytes, 128);
    assert_eq!(stop.stderr_bytes, 128, "what fits of the chunk is kept");

    let published = pollster::block_on(capture.finish(CallEnd::Exited(None)))
        .expect("finish succeeds")
        .expect("a stopped capture publishes what it kept");
    assert_eq!(
        read_block(&log, published.stderr.as_ref().unwrap()).len(),
        128
    );
    let record: Value =
        serde_json::from_slice(&read_block(&log, &published.record)).expect("record is JSON");
    assert_eq!(record["stdout"]["bytes"], 128);
    assert_eq!(record["stderr"]["bytes"], 128);
    assert_eq!(record["completeness"], "truncated");
}

/// A store that cannot hold the block is reported to the caller. The bounded
/// result is unaffected; what the caller must not do is believe the raw was
/// kept.
#[test]
fn test_publish_failure_reports_error() {
    struct NoBlocks;

    impl ContextBackend for NoBlocks {
        fn append(&self, _: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
            Box::pin(async { Err(ContextError::Unsupported) })
        }
        fn read_range(
            &self,
            _: SessionId,
            _: Option<EventId>,
            _: Option<EventId>,
        ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn replay(&self, _: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn write_checkpoint(
            &self,
            _: CheckpointManifest,
        ) -> PFut<'_, Result<CheckpointId, ContextError>> {
            Box::pin(async { Err(ContextError::Unsupported) })
        }
        fn read_checkpoint(
            &self,
            _: CheckpointId,
        ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
            Box::pin(async { Err(ContextError::Unsupported) })
        }
        fn list_checkpoints(
            &self,
            _: SessionId,
        ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    let log: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(NoBlocks)));
    let (_, capture) = begin(&log, None);
    capture.receive(CapturedStream::Stdout, b"raw output");
    let error = pollster::block_on(capture.finish(CallEnd::Exited(Some(0))))
        .expect_err("no store, no publication");
    assert!(
        matches!(error, CaptureError::StreamWrite(_)),
        "the error names the stage that failed: {error:?}"
    );
    assert!(
        error.to_string().contains("block write failed"),
        "the error reads as what failed: {error}"
    );
}

/// A store that takes both stream blocks but not the record reports the record
/// stage. Without the record nothing can authorize a read of the streams, so a
/// capture whose index is missing must not read as published.
#[test]
fn test_record_write_failure() {
    struct TwoBlocks {
        puts: AtomicUsize,
    }

    impl ContextBackend for TwoBlocks {
        fn append(&self, _: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
            Box::pin(async { Err(ContextError::Unsupported) })
        }
        fn read_range(
            &self,
            _: SessionId,
            _: Option<EventId>,
            _: Option<EventId>,
        ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn replay(&self, _: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn write_checkpoint(
            &self,
            _: CheckpointManifest,
        ) -> PFut<'_, Result<CheckpointId, ContextError>> {
            Box::pin(async { Err(ContextError::Unsupported) })
        }
        fn read_checkpoint(
            &self,
            _: CheckpointId,
        ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
            Box::pin(async { Err(ContextError::Unsupported) })
        }
        fn list_checkpoints(
            &self,
            _: SessionId,
        ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn block_put(&self, _: Vec<u8>) -> PFut<'_, Result<BlockHash, ContextError>> {
            Box::pin(async move {
                let put = self.puts.fetch_add(1, Ordering::SeqCst);
                if put < 2 {
                    Ok(BlockHash(format!("hash-{put}")))
                } else {
                    Err(ContextError::Unsupported)
                }
            })
        }
    }

    let log: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(TwoBlocks {
        puts: AtomicUsize::new(0),
    })));
    let (_, capture) = begin(&log, None);
    capture.receive(CapturedStream::Stdout, b"out");
    capture.receive(CapturedStream::Stderr, b"err");
    let error = pollster::block_on(capture.finish(CallEnd::Exited(Some(0))))
        .expect_err("a capture whose record is missing is not published");
    assert!(
        matches!(error, CaptureError::RecordWrite(_)),
        "the error names the stage that failed: {error:?}"
    );
    assert!(
        error.to_string().contains("record write failed"),
        "the error reads as what failed: {error}"
    );
}

/// A wall-budget end is not a finish: the record says the kept bytes are a
/// prefix and names the budget, so a reader does not take the tail for the
/// whole output. No status was observed, so none is written.
#[test]
fn test_wall_budget_marks_truncated() {
    let log = store();
    let (_, capture) = begin(&log, None);
    capture.receive(CapturedStream::Stdout, b"partial line");
    let published = pollster::block_on(capture.finish(CallEnd::WallBudget))
        .expect("finish succeeds")
        .expect("received output is published");
    assert!(
        published.stop.is_none(),
        "the capture stopped nothing itself"
    );
    assert_eq!(
        published.stop_reason,
        Some(CaptureStopReason::WallBudget),
        "the outcome names what ended the command"
    );
    let record: Value =
        serde_json::from_slice(&read_block(&log, &published.record)).expect("record is JSON");
    assert_eq!(record["completeness"], "truncated");
    assert_eq!(record["stop_reason"], json!("wall_budget"));
    assert!(record["exit"].is_null(), "nothing exited");
}

/// A command whose status was never observed is not complete either: a signal
/// can end it mid-stream, and the kept bytes are then only a prefix.
#[test]
fn test_missing_status_marks_truncated() {
    let log = store();
    let (_, capture) = begin(&log, None);
    capture.receive(CapturedStream::Stderr, b"stack trace");
    let published = pollster::block_on(capture.finish(CallEnd::Exited(None)))
        .expect("finish succeeds")
        .expect("received output is published");
    let record: Value =
        serde_json::from_slice(&read_block(&log, &published.record)).expect("record is JSON");
    assert_eq!(record["completeness"], "truncated");
    assert!(
        record["stop_reason"].is_null(),
        "no reason is claimed when none is known"
    );
}

/// A stopped capture reports the stop before it publishes, so a publication
/// that then fails still lets the caller say why the command ended.
#[test]
fn test_stop_readable_before_finish() {
    let log = store();
    let (_, capture) = begin(&log, Some(16));
    assert!(capture.stop().is_none(), "nothing stopped yet");
    assert!(matches!(
        capture.receive(CapturedStream::Stdout, &[b'a'; 32]),
        CaptureVerdict::Stop(_)
    ));
    let stop = capture.stop().expect("the applied stop is readable");
    assert_eq!(stop.reason, CaptureStopReason::ByteBudget);
    assert_eq!(stop.stdout_bytes, 16, "the prefix that fit is kept");
}
