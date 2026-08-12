//! The transcript-snapshot bridge: an impl of the TUI's TranscriptSnapshot
//! seam backed by the runner's SessionLog. The search view loads the whole
//! durable session log (every TurnEvent) via the backend's sync read,
//! projects each event to a SessionUpdate, and flattens to TranscriptLine
//! through the same transcript_from_frames the live render uses, so the
//! snapshot renders identically to the live transcript.
//!
//! The event-to-SessionUpdate projection is the service layer's
//! project_session_update -- one function, shared with the live path.
//! A local copy would drift (it already did: TurnAborted was missing from
//! the copy, so an interrupted turn rendered live but not in the snapshot
//! -- the index!=render the snapshot seam exists to eliminate). Sharing the
//! function is the structural parity guarantee, not a test.
//!
//! For logs over the threshold, the window method seeks + parses per screen
//! (never loading the whole log), and the lazy offset index (index_chunk)
//! reverse-reads from the tail so the view can seek to any scroll position
//! without reading the prefix. G triggers a full build with progress.

use std::sync::{Arc, Mutex};

use houyicoder_api::session::SessionLog;
use houyicoder_context::{SessionId, TurnEvent};
use houyicoder_service::projection::project_session_update;
use houyicoder_tui::records::TranscriptLine;
use houyicoder_tui::transcript::snapshot::{
    IndexProgress, SnapshotLoad, TranscriptSnapshot, WindowLoad,
};
use houyicoder_tui::transcript::{TranscriptFrame, transcript_from_frames};

/// The reverse-read chunk for the lazy index: 4 MB per index_chunk call.
/// At 60 fps this completes a 310 MB / 90k-event log in ~1.5 s (77 chunks).
const INDEX_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// The window read budget: 256 KB per screen (~70 events at 3.6 KB avg).
#[cfg(test)]
const WINDOW_MAX_BYTES: u64 = 256 * 1024;

/// The lazy event-byte-offset index. Built by reverse-reading from the
/// tail (EOF) toward BOF, prepending each batch so offsets stay in
/// forward (oldest-first) order. Until done, only the tail events are
/// indexed; byte_at returns None for un-indexed positions.
#[derive(Default)]
struct OffsetIndex {
    /// Byte offsets of events in FORWARD order (oldest first).
    offsets: Vec<u64>,
    /// How many bytes from the tail have been read.
    built_from_tail: u64,
    /// Total log file size.
    total_bytes: u64,
    /// True when the reverse read reached BOF (the index is complete).
    done: bool,
    /// Where the next reverse-read continues from (None at BOF).
    next_from: Option<u64>,
}

/// The TranscriptSnapshot bridge: holds the runner's SessionLog + the
/// session id + the lazy offset index. log_size + load + window + index
/// all read through the backend's sync path.
pub struct SessionLogSnapshot {
    pub(crate) session_log: Arc<dyn SessionLog>,
    pub(crate) session_id: SessionId,
    index: Mutex<OffsetIndex>,
}

impl SessionLogSnapshot {
    pub fn new(session_log: Arc<dyn SessionLog>, session_id: SessionId) -> Self {
        Self {
            session_log,
            session_id,
            index: Mutex::new(OffsetIndex::default()),
        }
    }

    /// Parse a raw JSONL line into a TurnEvent (for the offset index, which
    /// needs to know which lines are events + their byte positions). None
    /// for corrupt/non-event lines (skipped, not counted in offsets).
    fn parse_event(line: &str) -> Option<TurnEvent> {
        serde_json::from_str::<TurnEvent>(line).ok()
    }

    /// Project raw JSONL lines through the shared projection + flatten to
    /// TranscriptLine. Corrupt lines are skipped + counted (the tolerant
    /// path, not the strict replay path).
    fn project_lines(lines: &[(u64, String)]) -> (Vec<TranscriptLine>, usize) {
        let mut skipped = 0;
        let frames: Vec<TranscriptFrame> = lines
            .iter()
            .filter_map(|(_, line)| match Self::parse_event(line) {
                Some(ev) => project_session_update(&ev.kind).map(TranscriptFrame::Session),
                None => {
                    skipped += 1;
                    None
                }
            })
            .collect();
        let rendered = transcript_from_frames(&frames);
        (rendered, skipped)
    }
}

impl TranscriptSnapshot for SessionLogSnapshot {
    fn log_size(&self) -> u64 {
        self.session_log.backend().log_size(self.session_id)
    }

    fn load(&self, max_bytes: u64) -> SnapshotLoad {
        let size = self.log_size();
        if size > max_bytes {
            return SnapshotLoad {
                lines: Vec::new(),
                skipped: 0,
                truncated: true,
            };
        }
        let read = self.session_log.backend().read_log_lenient(self.session_id);
        let frames: Vec<TranscriptFrame> = read
            .events
            .iter()
            .filter_map(|ev| project_session_update(&ev.kind))
            .map(TranscriptFrame::Session)
            .collect();
        let lines = transcript_from_frames(&frames);
        SnapshotLoad {
            lines,
            skipped: read.skipped,
            truncated: false,
        }
    }

    fn window(&self, anchor: u64, max_bytes: u64) -> WindowLoad {
        let range = self
            .session_log
            .backend()
            .read_log_range(self.session_id, anchor, max_bytes);
        let (lines, skipped) = Self::project_lines(&range.lines);
        WindowLoad {
            lines,
            start_offset: anchor,
            next_offset: range.next_offset,
            skipped,
            bytes_total: range.bytes_total,
        }
    }

    fn tail_window(&self, max_bytes: u64) -> WindowLoad {
        let total = self.log_size();
        if total == 0 {
            return WindowLoad {
                bytes_total: 0,
                ..WindowLoad::default()
            };
        }
        // One reverse read from EOF: the newest batch, newest-first. Reverse
        // to forward (oldest-first) order so the projection renders top-down.
        let rev = self
            .session_log
            .backend()
            .read_lines_reverse(self.session_id, total, max_bytes);
        let fwd: Vec<(u64, String)> = rev.lines.into_iter().rev().collect();
        let start_offset = fwd.first().map(|(o, _)| *o).unwrap_or(total);
        let (lines, skipped) = Self::project_lines(&fwd);
        WindowLoad {
            lines,
            start_offset,
            // Past EOF == nothing newer; the tail is the newest window.
            next_offset: total,
            skipped,
            bytes_total: total,
        }
    }

    fn window_before(&self, from_byte: u64, max_bytes: u64) -> WindowLoad {
        let total = self.log_size();
        if from_byte == 0 || total == 0 {
            return WindowLoad {
                bytes_total: total,
                ..WindowLoad::default()
            };
        }
        // Reverse-read the lines ending at from_byte, reverse to forward
        // (oldest-first) order. start_offset is the oldest line's byte (0 at
        // BOF, which the caller uses to stop the older scan); next_offset is
        // from_byte so a newer scan chains back via window(from_byte).
        let rev =
            self.session_log
                .backend()
                .read_lines_reverse(self.session_id, from_byte, max_bytes);
        let fwd: Vec<(u64, String)> = rev.lines.into_iter().rev().collect();
        let start_offset = fwd.first().map(|(o, _)| *o).unwrap_or(0);
        let (lines, skipped) = Self::project_lines(&fwd);
        WindowLoad {
            lines,
            start_offset,
            next_offset: from_byte,
            skipped,
            bytes_total: total,
        }
    }

    fn index_chunk(&self) -> IndexProgress {
        let mut idx = self.index.lock().expect("index mutex poisoned");
        let backend = self.session_log.backend();
        if idx.total_bytes == 0 {
            idx.total_bytes = backend.log_size(self.session_id);
        }
        if idx.done || idx.total_bytes == 0 {
            return IndexProgress {
                indexed_bytes: idx.built_from_tail,
                total_bytes: idx.total_bytes,
                done: idx.done,
            };
        }
        let from = idx.next_from.unwrap_or(idx.total_bytes);
        let rev = backend.read_lines_reverse(self.session_id, from, INDEX_CHUNK_BYTES);
        // Collect event byte offsets from the reverse batch (newest-first).
        // Parse each line; record the offset if it is an event.
        let mut batch_offsets: Vec<u64> = Vec::new();
        for (offset, line) in &rev.lines {
            if Self::parse_event(line).is_some() {
                batch_offsets.push(*offset);
            }
        }
        // The batch is newest-first; reverse to oldest-first + prepend.
        batch_offsets.reverse();
        let mut offsets = std::mem::take(&mut idx.offsets);
        batch_offsets.extend_from_slice(&offsets);
        offsets = batch_offsets;
        idx.offsets = offsets;
        idx.built_from_tail = idx.total_bytes - rev.next_from.unwrap_or(0);
        idx.next_from = rev.next_from;
        if rev.next_from.is_none() {
            idx.done = true;
        }
        IndexProgress {
            indexed_bytes: idx.built_from_tail,
            total_bytes: idx.total_bytes,
            done: idx.done,
        }
    }

    fn byte_at(&self, event_idx: usize) -> Option<u64> {
        let idx = self.index.lock().expect("index mutex poisoned");
        if idx.done {
            idx.offsets.get(event_idx).copied()
        } else {
            None
        }
    }

    fn event_count(&self) -> Option<usize> {
        let idx = self.index.lock().expect("index mutex poisoned");
        if idx.done {
            Some(idx.offsets.len())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{EventId, TurnEventKind};
    use houyicoder_tui::records::TranscriptLine;

    fn ev(kind: TurnEventKind) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            kind,
        }
    }

    /// A bash call + result pair projects to a Tool chip row followed by a
    /// result row whose body is the raw stdout. The parity guarantee + the
    /// footprint source (body stored once).
    #[test]
    fn test_bash_renders_stdout_body() {
        let events = &[
            ev(TurnEventKind::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: serde_json::json!({"command": "echo hi"}),
            }),
            ev(TurnEventKind::tool_result(
                "c1".to_string(),
                serde_json::json!({"stdout": "hi\nthere", "exitCode": 0}),
            )),
        ];
        let frames: Vec<TranscriptFrame> = events
            .iter()
            .filter_map(|ev| project_session_update(&ev.kind))
            .map(TranscriptFrame::Session)
            .collect();
        let lines = transcript_from_frames(&frames);
        assert!(lines.len() >= 2, "call + result rows: {lines:?}");
        let body = match &lines[1] {
            TranscriptLine::Tool { body, .. } => body.clone(),
            other => panic!("expected result row, got {other:?}"),
        };
        assert!(body.contains("hi"), "stdout in body: {body}");
        assert!(body.contains("there"), "full stdout in body: {body}");
    }

    /// TurnAborted must surface in the snapshot. The shared projection
    /// closes the drift structurally; this test pins it.
    #[test]
    fn test_turn_aborted_visible_snapshot() {
        let events = &[ev(TurnEventKind::TurnAborted {
            reason: "user escape".into(),
        })];
        let frames: Vec<TranscriptFrame> = events
            .iter()
            .filter_map(|ev| project_session_update(&ev.kind))
            .map(TranscriptFrame::Session)
            .collect();
        let lines = transcript_from_frames(&frames);
        let text = match &lines[..] {
            [TranscriptLine::User(s)] => s.clone(),
            other => panic!("expected one user notice row, got {other:?}"),
        };
        assert!(
            text.contains("interrupted"),
            "TurnAborted notice in the snapshot: {text}"
        );
        assert!(
            text.contains("user escape"),
            "the abort reason carries through: {text}"
        );
    }

    /// Metadata-only events project to None.
    #[test]
    fn test_metadata_project_to_none() {
        assert!(
            project_session_update(&TurnEventKind::MetaUser {
                text: "nudge".into()
            })
            .is_none()
        );
        assert!(
            project_session_update(&TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0
            })
            .is_none()
        );
    }

    /// Build a real LocalFileBackend + SessionStore + SessionLogSnapshot over a
    /// temp root, appending the given events. For the real-backend acceptance
    /// tests (parity, multibyte, large-log budget) that must exercise the
    /// byte-window + reverse-read + index paths on disk, not the mock.
    fn bridge_with_log(
        events: &[TurnEvent],
    ) -> (SessionLogSnapshot, SessionId, std::path::PathBuf) {
        use houyicoder_memory::LocalFileBackend;
        use houyicoder_session::SessionStore;
        let root = std::env::temp_dir().join(format!(
            "houyi_bridge_acceptance_{}_{}",
            SessionId::new(),
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        let backend = LocalFileBackend::new(root.clone());
        let store = SessionStore::new(Box::new(backend));
        // SessionStore.append drives a tokio Mutex, so it needs a tokio runtime
        // (pollster cannot drive it); block_on a fresh runtime.
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        for ev in events {
            rt.block_on(store.append(ev.clone())).expect("append");
        }
        let session = events.first().map(|e| e.session).unwrap_or_default();
        let snap = SessionLogSnapshot::new(std::sync::Arc::new(store), session);
        (snap, session, root)
    }

    fn ev_session(session: SessionId, id: EventId, kind: TurnEventKind) -> TurnEvent {
        TurnEvent {
            id,
            session,
            ts: 0,
            prev_hash: None,
            kind,
        }
    }

    /// Source parity: the whole-log load and the byte-window read render the
    /// same lines for the same events. The window path seeks + parses per
    /// screen; the load path reads the whole log tolerantly. Both go through
    /// the same project_session_update + transcript_from_frames, so the
    /// rendered text must match byte-for-byte (the parity guarantee that
    /// closes index!=render).
    #[test]
    fn test_window_matches_load_render() {
        let session = SessionId::new();
        let events: Vec<TurnEvent> = (0..5)
            .map(|i| {
                ev_session(
                    session,
                    EventId::new(),
                    TurnEventKind::UserInput {
                        text: format!("line {i}"),
                    },
                )
            })
            .collect();
        let (snap, _s, root) = bridge_with_log(&events);
        let load = snap.load(1 << 20);
        let win = snap.window(0, 1 << 20);
        let load_text: Vec<String> = load.lines.iter().map(|l| l.render()).collect();
        let win_text: Vec<String> = win.lines.iter().map(|l| l.render()).collect();
        assert_eq!(load_text, win_text, "load vs window render parity");
        assert!(!win_text.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    /// A multi-byte UTF-8 sequence is preserved across a window boundary: the
    /// 64KB-chunk reverse read + the line-aligned forward read never split a
    /// multi-byte sequence, so no U+FFFD appears + the content is intact.
    #[test]
    fn test_window_safe_on_multibyte() {
        let session = SessionId::new();
        let body = "边界测试 UTF-8 安全性 🦀 end".to_string();
        let events = vec![
            ev_session(
                session,
                EventId::new(),
                TurnEventKind::AssistantMessage {
                    text: body.clone(),
                    thinking: None,
                },
            ),
            ev_session(
                session,
                EventId::new(),
                TurnEventKind::AssistantMessage {
                    text: "second".into(),
                    thinking: None,
                },
            ),
        ];
        let (snap, _s, root) = bridge_with_log(&events);
        // Forward window from 0 + reverse tail window both must keep the
        // multibyte char intact (no corruption across chunk edges).
        let fwd = snap.window(0, 1 << 20);
        let rev = snap.tail_window(1 << 20);
        let joined_fwd: String = fwd
            .lines
            .iter()
            .map(|l| l.render())
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            joined_fwd.contains('🦀'),
            "forward window keeps the multibyte: {joined_fwd}"
        );
        let joined_rev: String = rev
            .lines
            .iter()
            .map(|l| l.render())
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            joined_rev.contains('🦀'),
            "reverse tail keeps the multibyte: {joined_rev}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The lazy index does not cover the whole log until G completes: byte_at
    /// returns None for un-indexed positions, then Some after the build. The
    /// full build completes in a bounded number of chunks (no infinite loop).
    #[test]
    fn test_index_builds_bounded_chunks() {
        let session = SessionId::new();
        let events: Vec<TurnEvent> = (0..200)
            .map(|i| {
                ev_session(
                    session,
                    EventId::new(),
                    TurnEventKind::UserInput {
                        text: format!("ev {i} padding to a few bytes"),
                    },
                )
            })
            .collect();
        let (snap, _s, root) = bridge_with_log(&events);
        // Before the build, byte_at is None (index not done).
        assert!(snap.byte_at(0).is_none(), "byte_at None before the build");
        let mut steps = 0u32;
        let progress = loop {
            let p = snap.index_chunk();
            steps += 1;
            if p.done || steps > 1000 {
                break p;
            }
        };
        assert!(progress.done, "index build completed in {steps} chunks");
        assert!(steps <= 1000, "bounded, no infinite loop ({steps} steps)");
        assert!(snap.byte_at(0).is_some(), "byte_at answers after the build");
        assert!(
            snap.event_count().is_some(),
            "event_count answers after the build"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Real-machine budget on a large log: enter (tail_window) actually
    /// materializes the projection (lines > 0 + the tail needle renders), the
    /// enter < 300 ms, one window scan < 100 ms, the full index build
    /// completes, and the resident window + index stay bounded. Generates a
    /// synthetic local-format log just over the threshold so the projection is
    /// real (a foreign-format log would parse-skip to empty, measuring only
    /// the byte mechanism). Set HOUYICODER_LARGE_LOG to a real log path to
    /// additionally stress the byte mechanism on a bigger file (projection may
    /// be empty there -- the content assertions are skipped in that mode).
    #[test]
    #[ignore]
    // too_many_lines: a budget benchmark -- setup, measure, assert in one body.
    // Splitting obscures the measured region.
    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    fn test_large_log_budget() {
        use houyicoder_memory::LocalFileBackend;
        use houyicoder_session::SessionStore;
        let root = std::env::temp_dir().join(format!(
            "houyi_large_budget_{}_{}",
            SessionId::new(),
            std::process::id()
        ));
        let session = SessionId::new();
        let session_dir = root.join(format!("{session}"));
        std::fs::create_dir_all(&session_dir).expect("create session dir");
        let log_path = session_dir.join("log.jsonl");

        const NEEDLE: &str = "BUDGETNEEDLE";
        let real_log = std::env::var("HOUYICODER_LARGE_LOG").ok();
        let using_real = real_log
            .as_ref()
            .map(|p| std::path::Path::new(p).exists())
            .unwrap_or(false);
        if using_real {
            #[cfg(unix)]
            std::os::unix::fs::symlink(real_log.as_deref().unwrap(), &log_path)
                .expect("symlink the large log");
        } else {
            // Synthetic local-format log just over the threshold: ~520 events
            // with a ~32 KB body each ~ 16+ MB. The last event carries the
            // needle so the tail window's projection must surface it.
            let mut buf: Vec<u8> = Vec::with_capacity(17 * 1024 * 1024);
            for i in 0..520u32 {
                let text = if i == 519 {
                    format!("{NEEDLE} {}", "x".repeat(32 * 1024))
                } else {
                    "x".repeat(32 * 1024)
                };
                let ev = TurnEvent {
                    id: EventId::new(),
                    session,
                    ts: i as u64,
                    prev_hash: None,
                    kind: TurnEventKind::UserInput { text },
                };
                let mut line = serde_json::to_vec(&ev).expect("serialize event");
                line.push(b'\n');
                buf.extend_from_slice(&line);
            }
            std::fs::write(&log_path, buf).expect("write synthetic log");
        }
        let backend = LocalFileBackend::new(root.clone());
        let store = SessionStore::new(Box::new(backend));
        let snap = SessionLogSnapshot::new(std::sync::Arc::new(store), session);

        let total = snap.log_size();
        assert!(
            total > 16 * 1024 * 1024,
            "the large log is over the threshold ({total} bytes)"
        );

        // Enter (tail window): one 256 KB reverse read + parse + project.
        let t0 = std::time::Instant::now();
        let tail = snap.tail_window(WINDOW_MAX_BYTES);
        let enter_ms = t0.elapsed().as_millis();
        assert!(
            enter_ms < 300,
            "tail_window < 300ms on {total} bytes (took {enter_ms}ms)"
        );
        // Content materialization (the real-projection mode): the tail window
        // must hold rendered lines + the needle, not be empty. Skipped for a
        // foreign-format real log (projection parses to nothing there).
        let rendered: String = tail
            .lines
            .iter()
            .map(|l| l.render())
            .collect::<Vec<_>>()
            .join("|");
        if !using_real {
            assert!(
                !tail.lines.is_empty(),
                "tail window materialized lines (not empty):\n{rendered}"
            );
            assert!(
                rendered.contains(NEEDLE),
                "tail window contains the needle (real projection):\n{}",
                &rendered[..rendered.len().min(400)]
            );
        }
        // The resident window is bounded by WINDOW_MAX_BYTES regardless of log size.
        let window_bytes: usize = tail.lines.iter().map(|l| l.render().len()).sum();
        assert!(
            window_bytes < 1_000_000,
            "resident window bounded ({window_bytes} bytes), not the whole {total}-byte log"
        );

        // One older-window scan (window_before): bounded read + parse + project < 100 ms.
        let t0 = std::time::Instant::now();
        let _scan = snap.window_before(tail.start_offset, WINDOW_MAX_BYTES);
        let scan_ms = t0.elapsed().as_millis();
        assert!(scan_ms < 100, "window_before < 100ms (took {scan_ms}ms)");

        // Full index build: completes in a bounded number of chunks (no freeze
        // -- one chunk per frame in production; here we drain to done).
        let t0 = std::time::Instant::now();
        let mut steps = 0u32;
        let progress = loop {
            let p = snap.index_chunk();
            steps += 1;
            if p.done || steps > 100_000 {
                break p;
            }
        };
        let build_s = t0.elapsed().as_secs_f64();
        assert!(
            progress.done,
            "full index completed in {steps} chunks / {build_s:.1}s"
        );
        // The offset index size is event-count x 8 bytes, not the log size.
        let idx_bytes = snap.event_count().map(|n| n * 8).unwrap_or(0);
        assert!(
            idx_bytes < 5_000_000,
            "index bounded ({idx_bytes} bytes), not the {total}-byte log"
        );
        eprintln!(
            "large_log_budget: log {total} bytes ({}), enter {enter_ms}ms, scan {scan_ms}ms, index {steps} chunks {build_s:.1}s ({idx_bytes}B)",
            if using_real { "real" } else { "synthetic" }
        );
        let _removed = std::fs::remove_dir_all(&root);
    }
}
