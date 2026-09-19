//! The transcript-snapshot bridge: an impl of the TUI's TranscriptSnapshot
//! seam backed by the runner's SessionLog. The search view reads the durable
//! session log via the backend's sync read, maps each event to the frames the
//! live stream carries, and flattens them through the same
//! transcript_from_frames the live render uses, so scrolling through history
//! shows the turns the live view showed.
//!
//! The mappings are the service layer's map_session_update and
//! map_acpx_notification, the same two the live push uses. A local copy would
//! drift, as one already did when it left out the interruption notice.
//!
//! Run state is what the two paths cannot share: this one reads a log written
//! by another process, so a turn that no record closes stays open here.
//!
//! For logs over the threshold, the window method seeks + parses per screen
//! (never loading the whole log), and the lazy offset index (index_chunk)
//! reverse-reads from the tail so the view can seek to any scroll position
//! without reading the prefix. G triggers a full build with progress.

//! A window read walks a bounded lookback ahead of the window's first line, so
//! a window that starts inside a turn still carries that turn's summary row.
//! Rows are named within the frames a window loaded, so a row's name is stable
//! for one anchor and differs from the name the whole-log read gives it.

use std::sync::{Arc, Mutex};

use houyicoder_api::session::SessionLog;
use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use houyicoder_service::protocol_adapter::{map_acpx_notification, map_session_update};
use houyicoder_tui::records::TranscriptLine;
use houyicoder_tui::transcript::snapshot::{
    IndexProgress, SnapshotLoad, TranscriptSnapshot, WindowLoad,
};
use houyicoder_tui::transcript::{TranscriptFrame, bounds_turn_in, transcript_from_frames};

/// The reverse-read chunk for the lazy index: 4 MB per index_chunk call.
/// At 60 fps this completes a 310 MB / 90k-event log in ~1.5 s (77 chunks).
const INDEX_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// The window read budget: 256 KB per screen (~70 events at 3.6 KB avg).
#[cfg(test)]
const WINDOW_MAX_BYTES: u64 = 256 * 1024;

/// One reverse-read step of the turn lookback: the log ahead of a window is
/// read in 64 KB steps. A recorded turn spans 16 KB at the median and 66 KB at
/// the ninetieth percentile, so one step carries the turn a window starts
/// inside.
const LOOKBACK_STEP_BYTES: u64 = 64 * 1024;

/// The most a window read spends finding the turn it starts inside. A turn
/// whose opening message sits further back than this renders without its
/// summary row, as it did before the lookback existed.
const LOOKBACK_MAX_BYTES: u64 = 512 * 1024;

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

    /// Parse a raw JSONL line into a SessionLogEntry (for the offset index, which
    /// needs to know which lines are events + their byte positions). None
    /// for corrupt/non-event lines (skipped, not counted in offsets).
    fn parse_event(line: &str) -> Option<SessionLogEntry> {
        serde_json::from_str::<SessionLogEntry>(line).ok()
    }

    /// Map a durable event to the frames the projection reads, through the
    /// same two mappers the live push uses: the session/update stream and the
    /// acpx notifications it has no variant for. Both are needed — a run
    /// completion record arrives as a notification, and it is what tells the
    /// projection where a turn ended.
    fn frames_of(event: &SessionEvent) -> [Option<TranscriptFrame>; 2] {
        [
            map_session_update(event).map(TranscriptFrame::Session),
            map_acpx_notification(event).map(TranscriptFrame::Acpx),
        ]
    }

    /// The frames a run of durable events projects to. The snapshot has no run
    /// state to consult, so a turn its log carries no record for is left open:
    /// the snapshot never claims a turn ended that the log does not record as
    /// ended.
    fn frames_of_events<'a>(
        events: impl IntoIterator<Item = &'a SessionLogEntry>,
    ) -> Vec<TranscriptFrame> {
        let mut frames = Vec::new();
        for event in events {
            frames.extend(Self::frames_of(&event.event).into_iter().flatten());
        }
        frames
    }

    /// Project a run of durable events to transcript lines.
    fn project_events<'a>(
        events: impl IntoIterator<Item = &'a SessionLogEntry>,
    ) -> Vec<TranscriptLine> {
        let frames = Self::frames_of_events(events);
        transcript_from_frames(&frames, 0..frames.len(), true)
    }

    /// The frames a line's event projects to, in log order.
    fn frames_of_line(line: &str) -> Vec<TranscriptFrame> {
        Self::parse_event(line)
            .map(|ev| Self::frames_of(&ev.event).into_iter().flatten().collect())
            .unwrap_or_default()
    }

    /// Whether the step holds where a turn begins or ends, in the sense the
    /// projection reads: a message that opened a turn, or the record that
    /// closed one. The step's lines arrive newest first, so reading them in
    /// reverse walks the log forward and the frames of a message and of its
    /// delivery mark stay in the order the projection expects.
    fn holds_boundary(step: &[(u64, String)]) -> bool {
        let mut frames: Vec<TranscriptFrame> = Vec::new();
        for (_, line) in step.iter().rev() {
            frames.extend(Self::frames_of_line(line));
        }
        bounds_turn_in(&frames)
    }

    /// The lines of the log ahead of a window, read so the projection knows
    /// where the turn the window starts inside began. The read walks back in
    /// 64 KB steps and stops at the first step holding a turn boundary, so it
    /// costs the turn the window cuts into rather than the whole log.
    fn turn_lookback(&self, first_byte: u64) -> Vec<(u64, String)> {
        let backend = self.session_log.backend();
        let mut from = first_byte;
        let mut budget = LOOKBACK_MAX_BYTES;
        let mut newest_first: Vec<(u64, String)> = Vec::new();
        while from > 0 && budget > 0 {
            let step = budget.min(LOOKBACK_STEP_BYTES);
            let rev = backend.read_lines_reverse(self.session_id, from, step);
            budget -= step;
            let bounded = Self::holds_boundary(&rev.lines);
            newest_first.extend(rev.lines);
            if bounded {
                break;
            }
            from = rev.next_from.unwrap_or(0);
        }
        newest_first.reverse();
        newest_first
    }

    /// Project one screen: the window's own lines, folded against the frames
    /// the lookback recovered. A window starting inside a turn reaches the fold
    /// with the turn's opening frame behind its first line, which is what keeps
    /// the summary row the fold derives at the frame that closed the turn.
    /// Only the window's lines become rows; corrupt ones are skipped + counted.
    fn project_window(
        &self,
        first_byte: u64,
        lines: &[(u64, String)],
    ) -> (Vec<TranscriptLine>, usize) {
        let ahead = if first_byte > 0 {
            self.turn_lookback(first_byte)
        } else {
            Vec::new()
        };
        let ahead_events: Vec<SessionLogEntry> = ahead
            .iter()
            .filter_map(|(_, line)| Self::parse_event(line))
            .collect();
        let mut frames = Self::frames_of_events(&ahead_events);
        let start = frames.len();
        let mut skipped = 0;
        for (_, line) in lines {
            match Self::parse_event(line) {
                Some(ev) => frames.extend(Self::frames_of(&ev.event).into_iter().flatten()),
                None => skipped += 1,
            }
        }
        let window = transcript_from_frames(&frames, start..frames.len(), true);
        (window, skipped)
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
        let lines = Self::project_events(&read.events);
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
        let (lines, skipped) = self.project_window(anchor, &range.lines);
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
        // to forward (oldest-first) order so the mapping renders top-down.
        let rev = self
            .session_log
            .backend()
            .read_lines_reverse(self.session_id, total, max_bytes);
        let fwd: Vec<(u64, String)> = rev.lines.into_iter().rev().collect();
        let start_offset = fwd.first().map(|(o, _)| *o).unwrap_or(total);
        let (lines, skipped) = self.project_window(start_offset, &fwd);
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
        let (lines, skipped) = self.project_window(start_offset, &fwd);
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
#[path = "transcript_snapshot_bridge_tests.rs"]
mod tests;
