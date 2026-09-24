//! Tests for one turn's records, read when the drill asks: what the read
//! returns, what it refuses, and what it serves from its cache.

use super::super::reader::SessionLogTrajectory;
use super::super::view::records_of;
use super::paging::{disk_reader, disk_reader_at, pump, reader_of};
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_session::SessionStore;
use houyicoder_tui::state::{TrajectoryDrill, TrajectoryTurnKey};
use houyicoder_tui::view::trajectory_pane::{
    TrajectoryDetailState, TrajectoryDetailView, TrajectoryLog as _, TrajectoryRow, TrajectoryTurn,
};
use std::sync::Arc;
/// A drill reads the same records the list used to carry: the detail is the
/// turn's records, read from the bytes its key names.
#[test]
fn test_detail_matches_the_list() {
    let (store, reader, sid, _history, _root) = disk_reader_at(4);
    let view = pump(&reader);
    let turn = view
        .rows
        .iter()
        .find_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.clone()),
            _ => None,
        })
        .expect("a turn row");

    reader.request_detail(&drill_of(&turn));
    let detail = pump_detail(&reader, &turn.key);
    assert_eq!(
        detail.state,
        TrajectoryDetailState::Ready,
        "the read landed"
    );

    let events = store.trajectory_snapshot(sid);
    let expected = records_of(&events, turn.n);
    assert!(!expected.is_empty(), "the turn has records to compare");
    assert_eq!(
        detail.records, expected,
        "the detail is the turn's records, field for field"
    );
}

/// The identity a drill holds for a turn of the window.
fn drill_of(turn: &TrajectoryTurn) -> TrajectoryDrill {
    TrajectoryDrill {
        key: turn.key.clone(),
        number: turn.n,
        history_generation: 1,
    }
}

/// A detail read in flight, or one the source answers at once, is polled the
/// way a draw polls it.
fn pump_detail(
    reader: &SessionLogTrajectory,
    key: &TrajectoryTurnKey,
) -> Arc<TrajectoryDetailView> {
    for _ in 0..400 {
        let detail = reader.detail(key);
        if detail.state != TrajectoryDetailState::Loading {
            return detail;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the detail never landed");
}

/// Asking for another turn's records never serves the one before: a draw while
/// the new read is in flight shows the turn it asked for, or that it is still
/// reading, but not the previous turn's output under the new turn's header.
#[test]
fn test_detail_switch_keeps_none() {
    let (_store, reader, _sid, _history, _root) = disk_reader_at(4);
    let view = pump(&reader);
    let turns: Vec<TrajectoryTurn> = view
        .rows
        .iter()
        .filter_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.clone()),
            _ => None,
        })
        .collect();
    assert!(turns.len() >= 2, "the window holds turns to switch between");
    let (first, second) = (turns[0].clone(), turns[1].clone());

    reader.request_detail(&drill_of(&first));
    let ready = pump_detail(&reader, &first.key);
    assert_eq!(ready.state, TrajectoryDetailState::Ready);
    assert_eq!(ready.turn.as_ref().map(|turn| turn.n), Some(first.n));

    reader.request_detail(&drill_of(&second));
    let switched = reader.detail(&second.key);
    if let Some(turn) = switched.turn.as_ref() {
        assert_eq!(
            turn.n, second.n,
            "a view for the turn before is never served for this one"
        );
    }
    let landed = pump_detail(&reader, &second.key);
    assert_eq!(
        landed.turn.as_ref().map(|turn| turn.n),
        Some(second.n),
        "and the read lands on the turn that was asked for"
    );
}

/// A turn whose opening event is wider than one detail read can hold is a
/// failed read, not a key that names nothing: saying stale would blame the key
/// for the size of the event.
#[test]
fn test_detail_wide_opening_fails() {
    let (store, reader, sid) = disk_reader(3);
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    let wide_id = EventId::new();
    rt.block_on(store.append(SessionLogEntry {
        id: wide_id,
        session: sid,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "x".repeat(1100 * 1024),
        },
    }))
    .expect("append a wide opening event");
    drop(pump(&reader));

    let view = reader.trajectory();
    let wide = view
        .rows
        .iter()
        .filter_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.clone()),
            _ => None,
        })
        .find(|turn| turn.key.as_str() == wide_id.to_string())
        .expect("the wide turn is in the window");
    reader.request_detail(&drill_of(&wide));
    let detail = pump_detail(&reader, &wide.key);
    assert_eq!(
        detail.state,
        TrajectoryDetailState::Failed,
        "an unreadable opening event is a failed read: {:?}",
        detail.state
    );
}

/// A reader whose page reads work and whose detail reads panic: the window
/// loads, and a drill's worker dies without answering.
fn detail_panicking_reader() -> SessionLogTrajectory {
    use houyicoder_context::{
        CheckpointId, CheckpointManifest, ContextBackend, ContextError, LogRangeRead, ReverseRead,
    };

    type PFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

    struct PanicDetail(houyicoder_memory::LocalFileBackend);

    impl ContextBackend for PanicDetail {
        fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
            self.0.append(event)
        }
        fn read_range(
            &self,
            session: SessionId,
            from: Option<EventId>,
            to: Option<EventId>,
        ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            self.0.read_range(session, from, to)
        }
        fn replay(
            &self,
            session: SessionId,
        ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            self.0.replay(session)
        }
        fn write_checkpoint(
            &self,
            manifest: CheckpointManifest,
        ) -> PFut<'_, Result<CheckpointId, ContextError>> {
            self.0.write_checkpoint(manifest)
        }
        fn read_checkpoint(
            &self,
            id: CheckpointId,
        ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
            self.0.read_checkpoint(id)
        }
        fn list_checkpoints(
            &self,
            session: SessionId,
        ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
            self.0.list_checkpoints(session)
        }
        fn supports_log_windows(&self) -> bool {
            true
        }
        fn log_size(&self, session: SessionId) -> u64 {
            self.0.log_size(session)
        }
        fn read_lines_reverse(&self, session: SessionId, from: u64, max: u64) -> ReverseRead {
            self.0.read_lines_reverse(session, from, max)
        }
        fn read_log_range(&self, _session: SessionId, _from: u64, _max: u64) -> LogRangeRead {
            panic!("this detail read is broken")
        }
    }

    let root = std::env::temp_dir().join(format!(
        "houyi_detail_panic_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = Arc::new(SessionStore::new(Box::new(PanicDetail(
        houyicoder_memory::LocalFileBackend::new(root),
    ))));
    let sid = SessionId::new();
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    for i in 0..3u64 {
        rt.block_on(store.append(SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: i * 1000,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: format!("prompt {i}"),
            },
        }))
        .expect("append");
    }
    let (reader, _history) = reader_of(&store, sid);
    reader
}

/// A detail read whose worker dies is asked for again, and a run of them is
/// reported: a pane that kept serving the loading view of a read that never
/// arrived would stay on it for the rest of the session.
#[test]
fn test_detail_broken_read_fails() {
    let reader = detail_panicking_reader();
    let view = pump(&reader);
    let turn = view
        .rows
        .iter()
        .find_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.clone()),
            _ => None,
        })
        .expect("a turn row");
    reader.request_detail(&drill_of(&turn));
    let detail = pump_detail(&reader, &turn.key);
    assert_eq!(
        detail.state,
        TrajectoryDetailState::Failed,
        "a run of dead workers is reported: {:?}",
        detail.state
    );
}

/// A detail read that was in flight when the history was cleared is dropped:
/// the records describe a turn this session no longer has.
#[test]
fn test_detail_stale_after_clear() {
    let (store, reader, sid, _history, _root) = disk_reader_at(4);
    let view = pump(&reader);
    let turn = view
        .rows
        .iter()
        .find_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.clone()),
            _ => None,
        })
        .expect("a turn row");
    reader.request_detail(&drill_of(&turn));
    drop(pump_detail(&reader, &turn.key));

    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    let before = store.trajectory_head(sid).revision.epoch_event_id;
    // A clear is a mirror reset followed by the boundary event, which is what
    // makes the boundary the new epoch's first durable event.
    store.reset_trajectory(sid);
    rt.block_on(store.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 9_000,
        prev_hash: None,
        event: SessionEvent::ContextCleared { prior_turn: 4 },
    }))
    .expect("clear the session");
    let after = store.trajectory_head(sid).revision.epoch_event_id;
    assert_ne!(before, after, "a clear starts a new epoch");
    // The window reloads, so the history it holds is the new one.
    reader.load_earliest();
    drop(pump(&reader));

    // The window reloads a frame or two behind the clear, so the pane notices
    // the new history the way it does in a session: on a later draw.
    let mut seen = TrajectoryDetailState::Ready;
    for _ in 0..400 {
        seen = reader.detail(&turn.key).state;
        if seen == TrajectoryDetailState::Stale {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        seen,
        TrajectoryDetailState::Stale,
        "a detail read in another history is not this session's"
    );
}

/// A turn wider than one detail read comes back with its beginning and says so,
/// rather than showing part of a turn as the whole of it.
#[test]
fn test_detail_truncated() {
    let (store, reader, sid) = disk_reader(1);
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    for i in 0..4u64 {
        rt.block_on(store.append(SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: 1000 + i,
            prev_hash: None,
            event: SessionEvent::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({ "text": "y".repeat(400 * 1024) }),
                duration_ms: 5,
            },
        }))
        .expect("append");
    }
    drop(pump(&reader));

    let view = reader.trajectory();
    let turn = view
        .rows
        .iter()
        .find_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.clone()),
            _ => None,
        })
        .expect("a turn row");
    reader.request_detail(&drill_of(&turn));
    let detail = pump_detail(&reader, &turn.key);
    assert_eq!(detail.state, TrajectoryDetailState::Ready);
    assert!(
        detail.truncated,
        "a turn wider than the read says so instead of showing part of itself"
    );
}

/// A window that moves past the drilled turn does not take the records with it:
/// the detail was read from the bytes the drill captured.
#[test]
fn test_detail_survives_disk_eviction() {
    let (_store, reader, _sid, _history, _root) = disk_reader_at(400);
    let view = pump(&reader);
    let oldest = view
        .rows
        .iter()
        .find_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.clone()),
            _ => None,
        })
        .expect("a turn row");
    reader.request_detail(&drill_of(&oldest));
    let ready = pump_detail(&reader, &oldest.key);
    assert_eq!(ready.state, TrajectoryDetailState::Ready);

    for _ in 0..3 {
        reader.load_older();
        drop(pump(&reader));
    }
    let after = reader.detail(&oldest.key);
    assert_eq!(
        after.state,
        TrajectoryDetailState::Ready,
        "the records are still in hand after the window moved: {:?}",
        after.state
    );
    assert_eq!(after.turn.as_ref().map(|turn| turn.n), Some(oldest.n));
}

/// A mirror-backed reader answers a drill from the history it holds, and keeps
/// answering it: the same request is not re-read on every draw, and the view it
/// answers with is the one it built rather than a fresh fold.
#[test]
fn test_detail_mirror_caches() {
    let store = Arc::new(SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let sid = SessionId::new();
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    for i in 0..3u64 {
        rt.block_on(store.append(SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: i * 1000,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: format!("prompt {i}"),
            },
        }))
        .expect("append");
    }
    let (reader, history) = reader_of(&store, sid);
    let view = pump(&reader);
    let turn = view
        .rows
        .iter()
        .find_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.clone()),
            _ => None,
        })
        .expect("a turn row");

    let (_, _, reads_before) = history.read_stats();
    reader.request_detail(&drill_of(&turn));
    let first = reader.detail(&turn.key);
    assert_eq!(first.state, TrajectoryDetailState::Ready);
    let (_, _, reads_after_first) = history.read_stats();

    for _ in 0..50 {
        reader.request_detail(&drill_of(&turn));
        let detail = reader.detail(&turn.key);
        assert_eq!(
            detail.state,
            TrajectoryDetailState::Ready,
            "a mirror detail stays ready rather than being reported stale"
        );
        assert_eq!(detail.turn.as_ref().map(|turn| turn.n), Some(turn.n));
        assert!(
            std::sync::Arc::ptr_eq(&first, &detail),
            "the cached view is served, not folded again"
        );
    }
    let (_, _, reads_after) = history.read_stats();
    assert_eq!(
        reads_after_first, reads_after,
        "and answering it reads nothing"
    );
    let _ = reads_before;
}

/// A draw serves the detail it holds: once the records are in hand, asking for
/// them again reads nothing, however many frames go by.
#[test]
fn test_detail_draw_reads_nothing() {
    let (_store, reader, _sid, history, _root) = disk_reader_at(4);
    let view = pump(&reader);
    let turn = view
        .rows
        .iter()
        .find_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.clone()),
            _ => None,
        })
        .expect("a turn row");
    reader.request_detail(&drill_of(&turn));
    drop(pump_detail(&reader, &turn.key));

    let (_, reads_before, bytes_before) = history.read_stats();
    for _ in 0..1000 {
        reader.request_detail(&drill_of(&turn));
        drop(reader.detail(&turn.key));
    }
    let (_, reads_after, bytes_after) = history.read_stats();
    assert_eq!(
        (reads_before, bytes_before),
        (reads_after, bytes_after),
        "a settled detail is served from the cache, not read again"
    );
}

/// An anchor whose bytes no longer hold the event the key names is refused: a
/// log rewritten under the drill must not answer with another turn's records.
#[test]
fn test_detail_stale_anchor_refused() {
    let (store, reader, sid, _history, root) = disk_reader_at(4);
    let view = pump(&reader);
    let turn = view
        .rows
        .iter()
        .find_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.clone()),
            _ => None,
        })
        .expect("a turn row");

    // The log is replaced under the anchor the drill would capture: same shape,
    // different events.
    let mut body = String::new();
    for i in 0..4u64 {
        let entry = SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: i * 1000,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: format!("other {i}"),
            },
        };
        body.push_str(&serde_json::to_string(&entry).expect("serialize"));
        body.push('\n');
    }
    std::fs::write(root.join(sid.to_string()).join("log.jsonl"), body).expect("rewrite the log");
    store.reset_trajectory(sid);

    reader.request_detail(&drill_of(&turn));
    let detail = pump_detail(&reader, &turn.key);
    assert_eq!(
        detail.state,
        TrajectoryDetailState::Stale,
        "the bytes no longer describe the turn the key names"
    );
    assert!(
        detail.records.is_empty(),
        "and no other turn's records are served"
    );
}

/// A window that moves while a detail read is in flight does not affect it: the
/// bytes were captured when the drill asked, and the answer carries the turn
/// the drill named.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "a port delegate plus the test that drives it"
)]
fn test_detail_in_flight_eviction() {
    use houyicoder_context::{
        CheckpointId, CheckpointManifest, ContextBackend, ContextError, LogRangeRead, ReverseRead,
    };

    type PFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

    /// A backend whose detail reads wait for the test, so the window can move
    /// while one is in flight.
    struct Gated {
        inner: houyicoder_memory::LocalFileBackend,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        gated: std::sync::atomic::AtomicBool,
    }

    impl ContextBackend for Gated {
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
        fn replay(
            &self,
            session: SessionId,
        ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
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
        fn supports_log_windows(&self) -> bool {
            true
        }
        fn log_size(&self, session: SessionId) -> u64 {
            self.inner.log_size(session)
        }
        fn read_lines_reverse(&self, session: SessionId, from: u64, max: u64) -> ReverseRead {
            self.inner.read_lines_reverse(session, from, max)
        }
        fn read_log_range(&self, session: SessionId, from: u64, max: u64) -> LogRangeRead {
            // Only the first read waits: that is the drill's read, and the
            // window's own reads have to go through so it can move meanwhile.
            if !self.gated.swap(true, std::sync::atomic::Ordering::SeqCst) {
                self.entered.send(()).ok();
                self.release.lock().expect("release lock").recv().ok();
            }
            self.inner.read_log_range(session, from, max)
        }
    }

    let root = std::env::temp_dir().join(format!(
        "houyi_detail_gate_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let store = Arc::new(SessionStore::new(Box::new(Gated {
        inner: houyicoder_memory::LocalFileBackend::new(root),
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
        gated: std::sync::atomic::AtomicBool::new(false),
    })));
    let sid = SessionId::new();
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    for i in 0..300u64 {
        rt.block_on(store.append(SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: i * 1000,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: format!("prompt {i}"),
            },
        }))
        .expect("append");
    }
    let (reader, _history) = reader_of(&store, sid);
    let view = pump(&reader);
    let oldest = view
        .rows
        .iter()
        .find_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.clone()),
            _ => None,
        })
        .expect("a turn row");

    reader.request_detail(&drill_of(&oldest));
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the detail read starts");
    // The window walks away while the read is in flight.
    reader.load_older();
    drop(pump(&reader));
    drop(release_tx);

    let detail = pump_detail(&reader, &oldest.key);
    assert_eq!(
        detail.state,
        TrajectoryDetailState::Ready,
        "the read finishes against the bytes it captured"
    );
    assert_eq!(
        detail.turn.as_ref().map(|turn| turn.n),
        Some(oldest.n),
        "and answers for the turn the drill named"
    );
}
