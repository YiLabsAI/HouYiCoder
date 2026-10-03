//! The typed reader of a session's durable log: byte-anchored windows, a lazy
//! offset index, and a bounded turn lookback.
//!
//! A consumer projects the events it receives into its own view types. This
//! layer never renders, and it never holds the whole log: every read is
//! anchored to a byte offset and bounded by a budget.

use std::sync::{Arc, Mutex};

use houyicoder_api::session::SessionLog;
use houyicoder_context::{EventId, LenientRead, SessionEvent, SessionId, SessionLogEntry};

mod head;

/// The reverse-read chunk for the lazy index: 4 MB per index_chunk call.
/// At 60 fps this completes a 310 MB / 90k-event log in ~1.5 s (77 chunks).
const INDEX_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// One reverse-read step of the turn lookback: the log ahead of a window is
/// read in 64 KB steps. A recorded turn spans 16 KB at the median and 66 KB at
/// the ninetieth percentile, so one step carries the turn a window starts
/// inside.
pub(crate) const LOOKBACK_STEP_BYTES: u64 = 64 * 1024;

/// The most a window read spends finding the turn it starts inside. A turn
/// whose opening event sits further back than this renders without it. The
/// ceiling is the same magnitude as the byte budget the live transcript
/// keeps its resident frames under, though the two count different things:
/// the live budget estimates in-memory frames, this one bounds serialized
/// log bytes. The reverse walk stops at a turn boundary, so a read spends
/// the turn's own size; the ceiling is only reached when no boundary sits
/// within reach — a turn wider than the budget, or a stretch the boundary
/// probe cannot parse.
const LOOKBACK_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// The lazy event-byte-offset index. Built by reverse-reading from the
/// tail (EOF) toward BOF, prepending each batch so offsets stay in
/// forward (oldest-first) order. Until done, only the tail events are
/// indexed; byte_at returns None for un-indexed positions.
#[derive(Default)]
struct OffsetIndex {
    /// Byte offsets of events in FORWARD order (oldest first).
    offsets: Vec<u64>,
    /// Where each history began, by the event that began it, newest first.
    ///
    /// A clear starts a new history, and the walk that finds it reads the log
    /// backwards a chunk at a time. The scan the index already runs passes over
    /// those events, so it records them: a caller that asks where a history
    /// began can look here instead of walking the log again.
    epoch_starts: Vec<(EventId, u64)>,
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

/// One event with the byte offset of the line it came from.
///
/// The offset is what makes a window re-readable: a caller can hold it and ask
/// for exactly that event again, which a window cannot do from an index alone.
#[derive(Debug, Clone)]
pub(crate) struct LocatedEvent {
    pub byte_offset: u64,
    pub entry: SessionLogEntry,
}

/// A byte-anchored window of durable events, in forward (oldest-first) order.
///
/// lines_start_offset is where the first complete line begins, which may be a
/// line that turned out to be corrupt: it is the anchor a caller resumes from,
/// not the offset of the first event in events.
#[derive(Debug, Default, Clone)]
pub(crate) struct EventWindow {
    /// Parsed events in log order, each with its own offset.
    pub events: Vec<LocatedEvent>,
    /// Byte offset where this window's first complete line begins.
    pub lines_start_offset: u64,
    /// Byte offset just past the last complete line in this window.
    pub next_offset: u64,
    /// Total log file size at read time.
    pub bytes_total: u64,
    /// Corrupt or unparseable lines skipped in this window.
    pub skipped: usize,
    /// Complete lines this window returned, corrupt ones included. Zero means
    /// the read's budget could not reach a line's start, which is a different
    /// thing from a batch whose lines all failed to parse.
    pub lines_read: usize,
}

/// The most a page holds in memory once read.
pub(crate) const PAGE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// The most a page spends on the disk before it gives up. The walk doubles its
/// step to find a line wider than one read, and each attempt re-reads from the
/// same anchor, so the bytes a page actually pulls can be twice its resident
/// bound. The two are kept apart so the budget cannot claim to bound both.
pub(crate) const PAGE_READ_MAX_BYTES: u64 = 2 * PAGE_MAX_BYTES;

/// One reverse-read step of a page: the log is walked back in this much at a
/// time, so a page never holds more than the turns it needs.
pub(crate) const PAGE_STEP_BYTES: u64 = 256 * 1024;

/// The widest line one range read can name. The backend caps a single range
/// read at a mebibyte, so a wider line cannot be read whole by growing the
/// step: a caller that needs a whole line must give up instead of looping.
pub(crate) const MAX_READABLE_LINE_BYTES: u64 = 1024 * 1024;

/// A durable identity for one turn, and where its opening event sits.
///
/// The id is the identity; the offset is what makes the turn re-readable
/// without an index. Both are needed: the id alone cannot seek, and the offset
/// alone cannot say whether the bytes still describe the same turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TurnAnchor {
    pub user_input_id: houyicoder_context::EventId,
    pub byte_offset: u64,
}

/// A page of turns read backwards from a byte anchor.
#[derive(Debug, Default, Clone)]
pub(crate) struct TurnPage {
    /// The page's events in log order.
    pub events: Vec<LocatedEvent>,
    /// The turn the page starts at, or None when it starts at the log's first
    /// event. The anchor is what a caller holds to keep its place: a byte
    /// offset alone cannot say whether the bytes still describe that turn.
    pub oldest_anchor: Option<TurnAnchor>,
    /// True when the oldest turn in the page was cut short by the byte budget,
    /// so a caller must not present it as a whole turn.
    pub oldest_partial: bool,
    /// Lines in this page that did not parse. The page still holds the rest,
    /// so this is reported rather than treated as a failed read.
    pub skipped: usize,
    /// The byte the page ends at: the offset a later append starts from. The
    /// page is read backwards from here, so this is what says whether the bytes
    /// after the page are still the ones it stopped at.
    pub end_offset: u64,
}

impl TurnPage {
    /// Drop the fragment before the page's first turn.
    ///
    /// A page whose oldest turn was cut short by the byte budget starts inside
    /// a turn. What is left of it belongs to a turn the page does not hold, so
    /// it is dropped rather than drawn as a turn the session never had.
    pub(crate) fn drop_leading_fragment(&mut self) {
        if !self.oldest_partial {
            return;
        }
        match self
            .events
            .iter()
            .position(|event| is_user_input(&event.entry))
        {
            Some(cut) => {
                self.events.drain(..cut);
            }
            // No turn opens in the page at all: everything it holds belongs to
            // a turn it does not, so there is nothing left to draw.
            None => self.events.clear(),
        }
        self.oldest_partial = false;
    }

    /// How many turns this page opens. A page is read as a count of turns, so
    /// this is the unit a caller moves a window by: the tail page's count says
    /// what it hides, and an older page's count says how far back it reached.
    pub(crate) fn turn_count(&self) -> usize {
        self.events
            .iter()
            .filter(|event| is_user_input(&event.entry))
            .count()
    }
}

/// A typed reader over one session's durable log.
/// What a history reader actually asked the disk for.
///
/// A page's cost is claimed to be bounded, and a draw's is claimed to be zero.
/// Neither is provable from wall-clock time, so the reader counts what it did
/// and a test asserts on the counts.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct ReadStats {
    /// Whole-log reads. A paged read must never perform one.
    pub whole_reads: std::sync::atomic::AtomicUsize,
    /// Reverse and range reads, one per call to the backend.
    pub window_reads: std::sync::atomic::AtomicUsize,
    /// Bytes asked of the backend, which is what a budget can bound.
    pub requested_bytes: std::sync::atomic::AtomicU64,
}

pub(crate) struct SessionHistory {
    session_log: Arc<dyn SessionLog>,
    session_id: SessionId,
    /// Whether this session's log can be read as byte windows. Cached at
    /// construction: the answer belongs to the backend, and asking per call
    /// would put a stat on the draw path.
    byte_windows: bool,
    #[cfg(test)]
    stats: ReadStats,
    index: Mutex<OffsetIndex>,
}

impl SessionHistory {
    pub(crate) fn new(session_log: Arc<dyn SessionLog>, session_id: SessionId) -> Self {
        let byte_windows = session_log.backend().supports_log_windows();
        Self {
            session_log,
            session_id,
            byte_windows,
            #[cfg(test)]
            stats: ReadStats::default(),
            index: Mutex::new(OffsetIndex::default()),
        }
    }

    /// Whether this session's log can be read as byte windows, so a caller
    /// knows whether to page it or fall back to the in-memory mirror.
    pub(crate) fn byte_windows(&self) -> bool {
        self.byte_windows
    }

    /// The raw on-disk log size in bytes.
    pub(crate) fn log_size(&self) -> u64 {
        self.session_log.backend().log_size(self.session_id)
    }

    /// The whole log read leniently, for a log the caller has measured as
    /// under its size threshold. A corrupt line is skipped and counted rather
    /// than failing the read.
    pub(crate) fn read_whole_lenient(&self) -> LenientRead {
        #[cfg(test)]
        self.stats
            .whole_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.session_log.backend().read_log_lenient(self.session_id)
    }

    /// The reads this reader has performed, for a test to assert on.
    #[cfg(test)]
    pub(crate) fn read_stats(&self) -> (usize, usize, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.stats.whole_reads.load(Relaxed),
            self.stats.window_reads.load(Relaxed),
            self.stats.requested_bytes.load(Relaxed),
        )
    }

    /// Count one read of the log's bytes.
    fn note_window_read(&self, bytes: u64) {
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering::Relaxed;
            self.stats.window_reads.fetch_add(1, Relaxed);
            self.stats.requested_bytes.fetch_add(bytes, Relaxed);
        }
        #[cfg(not(test))]
        let _ = bytes;
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
        self.note_window_read(max_bytes.min(total));
        let rev = self
            .session_log
            .backend()
            .read_lines_reverse(self.session_id, total, max_bytes);
        let fwd: Vec<(u64, String)> = rev.lines.into_iter().rev().collect();
        let lines_start_offset = fwd.first().map(|(o, _)| *o).unwrap_or(total);
        let lines_read = fwd.len();
        let (events, skipped) = parse_lines(&fwd);
        EventWindow {
            events,
            lines_start_offset,
            lines_read,
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
        self.note_window_read(max_bytes.min(from_byte));
        let rev =
            self.session_log
                .backend()
                .read_lines_reverse(self.session_id, from_byte, max_bytes);
        let fwd: Vec<(u64, String)> = rev.lines.into_iter().rev().collect();
        let lines_start_offset = fwd.first().map(|(o, _)| *o).unwrap_or(0);
        let lines_read = fwd.len();
        let (events, skipped) = parse_lines(&fwd);
        EventWindow {
            events,
            lines_start_offset,
            lines_read,
            next_offset: from_byte,
            bytes_total: total,
            skipped,
        }
    }

    /// The events starting at a byte anchor, in forward order.
    pub(crate) fn window(&self, anchor: u64, max_bytes: u64) -> EventWindow {
        self.note_window_read(max_bytes);
        let range = self
            .session_log
            .backend()
            .read_log_range(self.session_id, anchor, max_bytes);
        let lines_read = range.lines.len();
        let (events, skipped) = parse_lines(&range.lines);
        EventWindow {
            events,
            lines_start_offset: anchor,
            lines_read,
            next_offset: range.next_offset,
            bytes_total: range.bytes_total,
            skipped,
        }
    }

    /// Whether the bytes at an anchor still describe the turn the anchor names.
    ///
    /// A byte offset alone cannot say: the log is append-only, so only a
    /// truncation or a rewrite moves the bytes, and a stale anchor read as if
    /// it were the turn it named would show the wrong history. The id is what
    /// makes the offset checkable, which is why an anchor carries both.
    pub(crate) fn anchor_holds(&self, anchor: TurnAnchor) -> bool {
        // The line at the anchor can be wider than one step, and a read whose
        // budget cannot reach its end returns nothing, so the step doubles
        // until it holds a line or the read is capped. Without this a wide
        // turn would read as a stale anchor and drop the window to the tail.
        let mut step = PAGE_STEP_BYTES;
        loop {
            let window = self.window(anchor.byte_offset, step);
            if let Some(event) = window.events.first() {
                return is_user_input(&event.entry) && event.entry.id == anchor.user_input_id;
            }
            if window.lines_read > 0 || step >= MAX_READABLE_LINE_BYTES {
                return false;
            }
            step = step.saturating_mul(2);
        }
    }

    /// The newest complete turns, read backwards from the end of the log.
    ///
    /// A page is a count of turns, not a byte budget: one turn can be larger
    /// than any fixed window, and a window can hold hundreds of short turns.
    /// The read walks back until it has seen one more turn than it keeps, so
    /// the oldest turn it returns is whole. max_bytes bounds the walk; when
    /// it runs out the oldest turn is cut, and the page says so rather than
    /// presenting a fragment as a turn.
    /// The complete turns immediately older than a byte anchor.
    ///
    /// The walk stops at the log's start and at a context clear, because a
    /// cleared session's trajectory is what came after the clear: reading past
    /// it would show turns the session no longer counts.
    pub(crate) fn turns_before(
        &self,
        from_byte: u64,
        page_turns: usize,
        max_bytes: u64,
    ) -> TurnPage {
        if from_byte == 0 || self.log_size() == 0 || page_turns == 0 {
            return TurnPage::default();
        }
        let mut anchor = from_byte;
        let mut budget = max_bytes;
        let mut io_bytes = 0u64;
        // Batches are kept apart and merged once: prepending each batch would
        // copy everything read so far, which costs more than the read itself
        // on a page that takes many steps.
        let mut batches: Vec<Vec<LocatedEvent>> = Vec::new();
        let mut opened = 0usize;
        let mut skipped = 0usize;
        let mut reached_start = false;
        let mut reached_clear = false;
        // One more turn than the page keeps: the extra opening marks where the
        // oldest kept turn begins, so the page can start there.
        let wanted = page_turns + 1;
        while opened < wanted && anchor > 0 && budget > 0 && io_bytes < PAGE_READ_MAX_BYTES {
            // A single event can be wider than one step, and a reverse read
            // whose budget cannot reach a line's start returns nothing, so the
            // step doubles until it holds a line or the budgets are spent. A
            // batch whose lines all failed to parse still counts as read, so
            // the walk moves past it instead of growing the step forever.
            let mut step = PAGE_STEP_BYTES.min(budget);
            let window = loop {
                // Clamped before the read, not after: the disk budget is what
                // bounds the walk, so an attempt that would cross it is not
                // made at all.
                let attempt = step
                    .min(anchor)
                    .min(PAGE_READ_MAX_BYTES.saturating_sub(io_bytes));
                if attempt == 0 {
                    break EventWindow::default();
                }
                io_bytes = io_bytes.saturating_add(attempt);
                let window = self.window_before(anchor, attempt);
                if window.lines_read > 0 {
                    break window;
                }
                if step >= budget || step >= anchor || io_bytes >= PAGE_READ_MAX_BYTES {
                    break window;
                }
                step = step.saturating_mul(2).min(budget);
            };
            if window.lines_read == 0 || window.lines_start_offset >= anchor {
                // Nothing left to parse, or no progress: stop rather than read
                // the same bytes again.
                break;
            }
            budget = budget.saturating_sub(anchor - window.lines_start_offset);
            skipped += window.skipped;
            opened += window
                .events
                .iter()
                .filter(|e| is_user_input(&e.entry))
                .count();
            reached_clear = window
                .events
                .iter()
                .any(|e| matches!(e.entry.event, SessionEvent::ContextCleared { .. }));
            batches.push(window.events);
            anchor = window.lines_start_offset;
            if anchor == 0 {
                reached_start = true;
            }
            if reached_clear {
                break;
            }
        }
        batches.reverse();
        let mut collected: Vec<LocatedEvent> =
            Vec::with_capacity(batches.iter().map(Vec::len).sum());
        for batch in batches {
            collected.extend(batch);
        }
        let oldest_partial = !reached_start && !reached_clear && opened <= page_turns;
        // The page starts at the clear when the walk reached one: what came
        // before belongs to a session view the log no longer counts. Otherwise
        // it starts at the opening of the oldest turn it keeps, which is one
        // opening further in than the page holds.
        let (keep_from, at_epoch_start) = if reached_clear {
            (
                collected
                    .iter()
                    .rposition(|e| matches!(e.entry.event, SessionEvent::ContextCleared { .. }))
                    .unwrap_or(0),
                true,
            )
        } else if opened > page_turns {
            (
                collected
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| is_user_input(&e.entry))
                    .map(|(i, _)| i)
                    .nth(opened - page_turns)
                    .unwrap_or(0),
                false,
            )
        } else {
            (0, reached_start)
        };
        let oldest_anchor = if at_epoch_start {
            None
        } else {
            collected
                .get(keep_from)
                .filter(|event| event.byte_offset > 0)
                .and_then(anchor_of)
        };
        TurnPage {
            events: collected.split_off(keep_from),
            oldest_anchor,
            oldest_partial,
            skipped,
            end_offset: from_byte,
        }
    }

    /// The events of the turn a window starts inside: read backwards from
    /// first_byte in steps until the predicate holds for a step, so the cost is
    /// the turn rather than the log. Returns the events in forward order.
    /// The predicate receives each step's events in forward order, because
    /// what counts as a turn boundary is the consumer's question: the
    /// transcript asks whether its rows bound a turn, the trajectory asks
    /// whether a user message opened one.
    pub(crate) fn lookback_until(
        &self,
        first_byte: u64,
        mut is_boundary: impl FnMut(&[LocatedEvent]) -> bool,
    ) -> Vec<LocatedEvent> {
        let backend = self.session_log.backend();
        let mut from = first_byte;
        let mut budget = LOOKBACK_MAX_BYTES;
        let mut newest_first: Vec<LocatedEvent> = Vec::new();
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
    /// Remember where a history began, once a walk has found it.
    ///
    /// A walk that finds the clear has paid for the answer, so the next reader
    /// of the same history is answered from here instead of walking again. The
    /// entry is a byte offset with the id that makes it meaningful, so a log
    /// rewritten under it is refused rather than read as another history.
    pub(crate) fn remember_epoch_start(&self, epoch: EventId, offset: u64) {
        let Ok(mut idx) = self.index.lock() else {
            return;
        };
        if idx.epoch_starts.iter().any(|(event, _)| *event == epoch) {
            return;
        }
        idx.epoch_starts.push((epoch, offset));
    }

    /// The byte where a history began, if it is known.
    ///
    /// The event that began an epoch is the newest clear before it, and a clear
    /// is where a walk stops. A start the index has seen, or one a walk has
    /// remembered, is answered from here: the walk is for what neither knows.
    fn known_epoch_start(&self, epoch: Option<EventId>) -> Option<u64> {
        let idx = self.index.lock().ok()?;
        match epoch {
            // The history the log itself began in starts at the first byte.
            None => idx.offsets.first().map(|_| 0),
            Some(id) => idx
                .epoch_starts
                .iter()
                .find(|(event, _)| *event == id)
                .map(|(_, offset)| *offset),
        }
    }

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
        let mut batch_epochs: Vec<(EventId, u64)> = Vec::new();
        for (offset, line) in &rev.lines {
            if let Some(entry) = parse_event(line) {
                batch_offsets.push(*offset);
                if matches!(entry.event, SessionEvent::ContextCleared { .. }) {
                    batch_epochs.push((entry.id, *offset));
                }
            }
        }
        // The scan is newest-first, so the batch's own finds are too.
        idx.epoch_starts.extend(batch_epochs);
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

/// Parse raw JSONL lines into located events, counting the ones that do not
/// parse.
fn parse_lines(lines: &[(u64, String)]) -> (Vec<LocatedEvent>, usize) {
    let mut events = Vec::with_capacity(lines.len());
    let mut skipped = 0;
    for (byte_offset, line) in lines {
        match parse_event(line) {
            Some(entry) => events.push(LocatedEvent {
                byte_offset: *byte_offset,
                entry,
            }),
            None => skipped += 1,
        }
    }
    (events, skipped)
}

/// The anchor of the turn an event opens, when it opens one.
fn anchor_of(event: &LocatedEvent) -> Option<TurnAnchor> {
    is_user_input(&event.entry).then_some(TurnAnchor {
        user_input_id: event.entry.id,
        byte_offset: event.byte_offset,
    })
}

/// Whether an event opens a user turn, the boundary a page counts.
pub(crate) fn is_user_input(entry: &SessionLogEntry) -> bool {
    matches!(entry.event, SessionEvent::UserInput { .. })
}

/// How many turns the events open.
#[cfg(test)]
fn turns_opened(events: &[LocatedEvent]) -> usize {
    events
        .iter()
        .filter(|event| is_user_input(&event.entry))
        .count()
}

/// Parse one raw JSONL line into an event. None for a corrupt or non-event
/// line, which callers skip and count rather than treating as a boundary.
fn parse_event(line: &str) -> Option<SessionLogEntry> {
    serde_json::from_str::<SessionLogEntry>(line).ok()
}

#[cfg(test)]
#[path = "session_history_tests.rs"]
mod tests;
