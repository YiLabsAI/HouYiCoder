//! The one typed reader of a session's durable log.
//!
//! The transcript snapshot and the trajectory view share the byte windows,
//! the lazy offset index, and the bounded turn lookback through this type,
//! rather than each growing its own copy. A consumer projects the events it
//! receives into its own view types; this layer never renders, and it never
//! holds the whole log: every read is anchored to a byte offset and bounded by
//! a budget.

use std::sync::{Arc, Mutex};

use houyicoder_api::session::SessionLog;
use houyicoder_context::{LenientRead, SessionId, SessionLogEntry};

/// The reverse-read chunk for the lazy index: 4 MB per index_chunk call.
/// At 60 fps this completes a 310 MB / 90k-event log in ~1.5 s (77 chunks).
const INDEX_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// One reverse-read step of the turn lookback: the log ahead of a window is
/// read in 64 KB steps. A recorded turn spans 16 KB at the median and 66 KB at
/// the ninetieth percentile, so one step carries the turn a window starts
/// inside.
pub(crate) const LOOKBACK_STEP_BYTES: u64 = 64 * 1024;

/// The most a window read spends finding the turn it starts inside. A turn
/// whose opening event sits further back than this renders without it.
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

/// How much of the event-byte-offset index is built.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistoryIndexProgress {
    /// Bytes of the log indexed so far (from the tail backward).
    pub indexed_bytes: u64,
    /// Total log file size.
    pub total_bytes: u64,
    /// True when the full index is built.
    pub done: bool,
}

/// A byte-anchored window of durable events, in forward (oldest-first) order.
///
/// start_offset is where the first event begins; next_offset is just past the
/// last one. Corrupt lines are counted rather than returned, so a caller can
/// surface the gap without losing the rest of the window.
#[derive(Debug, Default, Clone)]
pub(crate) struct EventWindow {
    /// Parsed events in log order.
    pub events: Vec<SessionLogEntry>,
    /// Byte offset where the first event in this window begins.
    pub start_offset: u64,
    /// Byte offset just past the last event in this window.
    pub next_offset: u64,
    /// Total log file size at read time.
    pub bytes_total: u64,
    /// Corrupt or unparseable lines skipped in this window.
    pub skipped: usize,
}

/// A typed reader over one session's durable log.
pub(crate) struct SessionHistory {
    session_log: Arc<dyn SessionLog>,
    session_id: SessionId,
    index: Mutex<OffsetIndex>,
}

impl SessionHistory {
    pub(crate) fn new(session_log: Arc<dyn SessionLog>, session_id: SessionId) -> Self {
        Self {
            session_log,
            session_id,
            index: Mutex::new(OffsetIndex::default()),
        }
    }

    /// The raw on-disk log size in bytes.
    pub(crate) fn log_size(&self) -> u64 {
        self.session_log.backend().log_size(self.session_id)
    }

    /// The whole log read leniently, for a log the caller has measured as
    /// under its size threshold. A corrupt line is skipped and counted rather
    /// than failing the read.
    pub(crate) fn read_whole_lenient(&self) -> LenientRead {
        self.session_log.backend().read_log_lenient(self.session_id)
    }

    /// The newest events the log holds, as a window: the tail read is
    /// anchored at EOF and bounded by max_bytes.
    pub(crate) fn tail_window(&self, max_bytes: u64) -> EventWindow {
        let total = self.log_size();
        if total == 0 {
            return EventWindow {
                bytes_total: 0,
                ..EventWindow::default()
            };
        }
        // One reverse read from EOF: the newest batch, newest-first. Reverse
        // to forward order so a consumer renders top-down.
        let rev = self
            .session_log
            .backend()
            .read_lines_reverse(self.session_id, total, max_bytes);
        let fwd: Vec<(u64, String)> = rev.lines.into_iter().rev().collect();
        let start_offset = fwd.first().map(|(o, _)| *o).unwrap_or(total);
        let (events, skipped) = parse_lines(&fwd);
        EventWindow {
            events,
            start_offset,
            // Past EOF == nothing newer; the tail is the newest window.
            next_offset: total,
            bytes_total: total,
            skipped,
        }
    }

    /// The events immediately OLDER than from_byte, in forward order.
    pub(crate) fn window_before(&self, from_byte: u64, max_bytes: u64) -> EventWindow {
        let total = self.log_size();
        if from_byte == 0 || total == 0 {
            return EventWindow {
                bytes_total: total,
                ..EventWindow::default()
            };
        }
        let rev =
            self.session_log
                .backend()
                .read_lines_reverse(self.session_id, from_byte, max_bytes);
        let fwd: Vec<(u64, String)> = rev.lines.into_iter().rev().collect();
        let start_offset = fwd.first().map(|(o, _)| *o).unwrap_or(0);
        let (events, skipped) = parse_lines(&fwd);
        EventWindow {
            events,
            start_offset,
            next_offset: from_byte,
            bytes_total: total,
            skipped,
        }
    }

    /// The events starting at a byte anchor, in forward order.
    pub(crate) fn window(&self, anchor: u64, max_bytes: u64) -> EventWindow {
        let range = self
            .session_log
            .backend()
            .read_log_range(self.session_id, anchor, max_bytes);
        let (events, skipped) = parse_lines(&range.lines);
        EventWindow {
            events,
            start_offset: anchor,
            next_offset: range.next_offset,
            bytes_total: range.bytes_total,
            skipped,
        }
    }

    /// The events of the turn a window starts inside: read backwards from
    /// first_byte in steps until the predicate holds for a step, so the cost is
    /// the turn rather than the log. Returns the events in forward order.
    ///
    /// The predicate receives each step's events in forward order, because
    /// what counts as a turn boundary is the consumer's question: the
    /// transcript asks whether its rows bound a turn, the trajectory asks
    /// whether a user message opened one.
    pub(crate) fn lookback_until(
        &self,
        first_byte: u64,
        mut is_boundary: impl FnMut(&[SessionLogEntry]) -> bool,
    ) -> Vec<SessionLogEntry> {
        let backend = self.session_log.backend();
        let mut from = first_byte;
        let mut budget = LOOKBACK_MAX_BYTES;
        let mut newest_first: Vec<SessionLogEntry> = Vec::new();
        while from > 0 && budget > 0 {
            let step = budget.min(LOOKBACK_STEP_BYTES);
            let rev = backend.read_lines_reverse(self.session_id, from, step);
            budget -= step;
            let fwd: Vec<(u64, String)> = rev.lines.iter().rev().cloned().collect();
            let (events, _) = parse_lines(&fwd);
            let bounded = is_boundary(&events);
            newest_first.extend(events.into_iter().rev());
            if bounded {
                break;
            }
            from = rev.next_from.unwrap_or(0);
        }
        newest_first.reverse();
        newest_first
    }

    /// Build the next chunk of the event-byte-offset index. Called per frame
    /// while a full scan is asked for; a no-op once complete.
    pub(crate) fn index_chunk(&self) -> HistoryIndexProgress {
        let mut idx = self.index.lock().expect("index mutex poisoned");
        let backend = self.session_log.backend();
        if idx.total_bytes == 0 {
            idx.total_bytes = backend.log_size(self.session_id);
        }
        if idx.done || idx.total_bytes == 0 {
            return HistoryIndexProgress {
                indexed_bytes: idx.built_from_tail,
                total_bytes: idx.total_bytes,
                done: idx.done,
            };
        }
        let from = idx.next_from.unwrap_or(idx.total_bytes);
        let rev = backend.read_lines_reverse(self.session_id, from, INDEX_CHUNK_BYTES);
        // Collect event byte offsets from the reverse batch (newest-first),
        // then prepend so the index stays in forward order.
        let mut batch_offsets: Vec<u64> = Vec::new();
        for (offset, line) in &rev.lines {
            if parse_event(line).is_some() {
                batch_offsets.push(*offset);
            }
        }
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
        HistoryIndexProgress {
            indexed_bytes: idx.built_from_tail,
            total_bytes: idx.total_bytes,
            done: idx.done,
        }
    }

    /// The byte offset of the event at event_idx, once the index covers it.
    pub(crate) fn byte_at(&self, event_idx: usize) -> Option<u64> {
        let idx = self.index.lock().expect("index mutex poisoned");
        if idx.done {
            idx.offsets.get(event_idx).copied()
        } else {
            None
        }
    }

    /// The total event count, once the index is complete.
    pub(crate) fn event_count(&self) -> Option<usize> {
        let idx = self.index.lock().expect("index mutex poisoned");
        if idx.done {
            Some(idx.offsets.len())
        } else {
            None
        }
    }
}

/// Parse raw JSONL lines into events, counting the ones that do not parse.
fn parse_lines(lines: &[(u64, String)]) -> (Vec<SessionLogEntry>, usize) {
    let mut events = Vec::with_capacity(lines.len());
    let mut skipped = 0;
    for (_, line) in lines {
        match parse_event(line) {
            Some(event) => events.push(event),
            None => skipped += 1,
        }
    }
    (events, skipped)
}

/// Parse one raw JSONL line into an event. None for a corrupt or non-event
/// line, which callers skip and count rather than treating as a boundary.
fn parse_event(line: &str) -> Option<SessionLogEntry> {
    serde_json::from_str::<SessionLogEntry>(line).ok()
}
