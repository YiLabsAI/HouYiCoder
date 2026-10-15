//! Behavioral tests for incremental transcript rebuilding.
//!
//! New frames render immediately. Stable history is reused while the current
//! turn is rebuilt, and rewind, replay, and history loading preserve ordering.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::thread;
use std::time::{Duration, Instant};

use crate::agent_message::ServerEvent;
use crate::agent_message::SessionMessage;
use crate::composition;
use crate::fold::compute_fold_groups;
use crate::records::{ContextSuggestion, SuggestionSeverity, TranscriptLine};
use crate::session::SessionConnection;
use crate::state::transcript::DiskFront;
use crate::state::{App, Screen};
use crate::test_harness::MockSnapshot;
use crate::todo_view::TodoStatus;
use crate::transcript::NamedRows;
use crate::transcript::TranscriptFrame;
use crate::transcript::snapshot::{
    IndexProgress, ReadError, SnapshotLoad, TranscriptSnapshot, WindowLoad,
};
use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use serde_json::json;
use std::sync::mpsc::channel;

fn user_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}
fn agent_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}
fn tool_call(id: &str, tool: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, tool)
            .raw_input(json!({}))
            .status(ToolCallStatus::InProgress),
    ))
}
fn tool_result(id: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id,
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(json!({"ok": true})),
    )))
}
fn todo_write_frame(id: &str, todos: &[(&str, &str)]) -> TranscriptFrame {
    let arr = todos
        .iter()
        .map(|(c, s)| json!({"content": c, "status": s}))
        .collect::<Vec<_>>();
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, "todo_write")
            .raw_input(json!({"todos": arr}))
            .status(ToolCallStatus::InProgress),
    ))
}
/// Return an App with empty transcript state and a test connection so the
/// history read dispatches to a background task the test then pumps.
fn fresh_app() -> App {
    let mut app = composition::app();
    app.transcript_mut().reset();
    let (_tx, rx) = channel::<SessionMessage>();
    app.sessions.current.connection = Some(SessionConnection::from_receiver(rx));
    app
}
/// Pump the history read on file until it lands or times out. Mirrors what the
/// app loop does each pass, but blocks the test thread so the result is applied
/// before assertions run. Returns false at once when no read was dispatched
/// (the resident-frame path or a no-op short-circuit).
///
/// The question is whether a record is on file, not whether a worker runs: a
/// read can have finished and left its result waiting for the pump.
fn pump_history_read_blocking(app: &mut App, timeout: Duration) -> bool {
    if !app.sessions.current.history_reads.holds_pending() {
        return false;
    }
    let start = Instant::now();
    while start.elapsed() < timeout {
        if app.pump_history_read() {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}
/// Wait for the dispatched workers to leave, bounded so one that never returns
/// fails the test instead of hanging it. A worker sends its result before it
/// exits, so the count right after a landing can still see it on its way out.
fn wait_for_workers_to_leave(app: &App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.sessions.current.history_reads.running() > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
}
fn pump(app: &mut App, frame: TranscriptFrame) {
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(frame.into())));
}
fn compaction_frame() -> TranscriptFrame {
    TranscriptFrame::Acpx(AcpxNotification::new(
        AcpxMethod::ContextCompactionBoundary,
        json!({ "checkpoint": "01J00000000000000000000000" }),
    ))
}
/// The incrementally maintained fold cache must agree with a full recompute
/// after an append (tail-only recompute) and after a turn boundary (prefix
/// re-derive). The signature is start, end, key, active; stats, hint, and git
/// ops derive from the same lines, so they cannot diverge once the boundaries
/// match.
fn fold_signature(app: &App) -> Vec<(usize, usize, String, bool)> {
    app.transcript()
        .fold_groups()
        .iter()
        .map(|g| (g.start, g.end, g.key.clone(), g.active))
        .collect()
}
fn recompute_fold_signature(app: &App) -> Vec<(usize, usize, String, bool)> {
    compute_fold_groups(app.transcript().lines(), app.agent_busy())
        .iter()
        .map(|g| (g.start, g.end, g.key.clone(), g.active))
        .collect()
}
/// An App whose frame window was drained past its start: the oldest frames are
/// gone, so a scroll back to the front has only the session log to read from.
fn drained_app() -> App {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().set_resident_byte_budget(4096);
    let text = "x".repeat(200);
    for i in 0..40 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("{i} {text}")));
    }
    app.rebuild_transcript();
    // The pass that drained the frames still renders their rows. A second
    // rebuild derives the view from the resident window alone, and the tests
    // read that settled view.
    app.rebuild_transcript();
    let settled = app.transcript().len();
    app.rebuild_transcript();
    assert_eq!(
        app.transcript().len(),
        settled,
        "the view settles after the drain"
    );
    assert!(
        app.transcript().frame_window_start() > 0,
        "the budget drained the oldest frames"
    );
    app
}
/// One window of the session log, as the read port returns it. The rows carry
/// positional identities, so an overlap match against them falls back to text.
fn log_window(lines: Vec<TranscriptLine>, start_offset: u64, next_offset: u64) -> WindowLoad {
    let rows = NamedRows::from_lines(lines);
    WindowLoad {
        lines: rows.lines,
        row_ids: rows.ids,
        start_offset,
        next_offset,
        skipped: 0,
        bytes_total: 4096,
    }
}
/// A log source returning the given windows, newest last.
fn log_source(windows: Vec<WindowLoad>) -> Arc<MockSnapshot> {
    Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 4096,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows,
        index_steps: 0,
        index_calls: AtomicU32::new(0),
    })
}

/// A source whose older-window reads fail, delegating everything else.
struct FailingWindows {
    inner: Arc<MockSnapshot>,
}

/// A log source every window read of which fails with an IO error.
fn failing_log_source() -> Arc<FailingWindows> {
    Arc::new(FailingWindows {
        inner: log_source(Vec::new()),
    })
}

impl TranscriptSnapshot for FailingWindows {
    fn log_size(&self) -> u64 {
        self.inner.log_size()
    }
    fn load(&self, max_bytes: u64) -> SnapshotLoad {
        self.inner.load(max_bytes)
    }
    fn window(&self, anchor: u64, max_bytes: u64) -> WindowLoad {
        self.inner.window(anchor, max_bytes)
    }
    fn tail_window(&self, max_bytes: u64) -> Result<WindowLoad, ReadError> {
        self.inner.tail_window(max_bytes)
    }
    fn window_before(&self, _from_byte: u64, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
        Err(ReadError::Io)
    }
    fn index_chunk(&self) -> IndexProgress {
        self.inner.index_chunk()
    }
    fn byte_at(&self, event_idx: usize) -> Option<u64> {
        self.inner.byte_at(event_idx)
    }
    fn event_count(&self) -> Option<usize> {
        self.inner.event_count()
    }
}
/// Every row the view holds prints once: an overlap that kept the rows the tail
/// read repeats would show them twice, one that cut short would lose them.
fn assert_rows_print_once(app: &App) {
    let mut texts: Vec<String> = app.transcript().iter().map(|l| l.render()).collect();
    let total = texts.len();
    texts.sort();
    texts.dedup();
    assert_eq!(
        texts.len(),
        total,
        "a row prints twice: {:?}",
        app.transcript()
    );
}

mod eviction;
mod history;
mod rebuild;
mod scrollback;
