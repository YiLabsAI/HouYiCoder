//! The typed reader of a session's durable log: byte-anchored windows, a
//! history-epoch scan, and a bounded turn lookback.
//!
//! A consumer folds the events it receives into its own view types. This
//! layer never renders, and it never holds the whole log: every read is
//! anchored to a byte offset and bounded by a budget. Record offsets are
//! answered from the index the log carries beside it, so no per-record table
//! lives in memory either.

use std::sync::{Arc, Mutex};

use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    ContextError, EventId, LenientRead, SessionEvent, SessionId, SessionLogEntry,
};
#[cfg(test)]
use std::sync::atomic::AtomicU64;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
use std::sync::atomic::Ordering::Relaxed;

mod head;
pub(crate) mod log_index;

/// One reverse-read chunk of the epoch scan: the scan reads the log backward
/// by this much per call. At 60 fps this completes a 310 MB / 90k-event scan
/// in ~1.5 s (77 chunks).
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

/// The state of the reverse scan that discovers where histories began.
///
/// The scan reads the log backward from EOF a chunk at a time, records every
/// clear it passes, and reports progress to whoever asked for the full scan.
/// Record offsets do not live here: they are answered from the index the log
/// carries beside it, so this state stays the same size over any log.
#[derive(Default)]
struct EpochScan {
    /// Where each history began, by the event that began it, newest first.
    ///
    /// A clear starts a new history, and the walk that finds it reads the log
    /// backwards a chunk at a time. The scan already passes over those events,
    /// so it records them: a caller that asks where a history began can look
    /// here instead of walking the log again.
    epoch_starts: Vec<(EventId, u64)>,
    /// How many bytes from the tail have been read.
    built_from_tail: u64,
    /// Total log file size.
    total_bytes: u64,
    /// True when the reverse read reached BOF (the scan is complete).
    done: bool,
    /// Where the next reverse-read continues from (None at BOF).
    next_from: Option<u64>,
    /// Whether the log's sidecar index has had its one check. The check runs
    /// on the first scan chunk, which a worker asks for, rather than on a
    /// query path, and a log whose index is missing or refused takes its one
    /// streaming rebuild there. One check per scan, not per chunk: a log
    /// whose rebuild was refused would pay the whole streaming pass again on
    /// every chunk, and a transient failure gets its retry from the next
    /// reader built over the log anyway.
    sidecar_checked: bool,
}

/// How far the epoch scan's reverse read has gotten.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistoryIndexProgress {
    /// Bytes of the log the reverse scan has read (from the tail backward).
    pub indexed_bytes: u64,
    /// Total log file size.
    pub total_bytes: u64,
    /// True when the scan reached the beginning of the log.
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

/// What a history reader actually asked the disk for.
///
/// A page's cost is claimed to be bounded, and a draw's is claimed to be zero.
/// Neither is provable from wall-clock time, so the reader counts what it did
/// and a test asserts on the counts.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct ReadStats {
    /// Whole-log reads. A paged read must never perform one.
    pub whole_reads: AtomicUsize,
    /// Reverse and range reads, one per call to the backend.
    pub window_reads: AtomicUsize,
    /// Bytes asked of the backend, which is what a budget can bound.
    pub requested_bytes: AtomicU64,
}

/// A typed reader over one session's durable log.
pub(crate) struct SessionHistory {
    session_log: Arc<dyn SessionLog>,
    session_id: SessionId,
    /// Whether this session's log can be read as byte windows. Cached at
    /// construction: the answer belongs to the backend, and asking per call
    /// would put a stat on the draw path.
    byte_windows: bool,
    #[cfg(test)]
    stats: ReadStats,
    scan: Mutex<EpochScan>,
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
            scan: Mutex::new(EpochScan::default()),
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
        self.stats.whole_reads.fetch_add(1, Relaxed);
        self.session_log.backend().read_log_lenient(self.session_id)
    }

    /// The reads this reader has performed, for a test to assert on.
    #[cfg(test)]
    pub(crate) fn read_stats(&self) -> (usize, usize, u64) {
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
            self.stats.window_reads.fetch_add(1, Relaxed);
            self.stats.requested_bytes.fetch_add(bytes, Relaxed);
        }
        #[cfg(not(test))]
        let _ = bytes;
    }

    /// The newest events the log holds, as a window: the tail read is
    /// anchored at EOF and bounded by max_bytes.
    pub(crate) fn tail_window(&self, max_bytes: u64) -> Result<EventWindow, ContextError> {
        let total = self.log_size();
        if total == 0 {
            return Ok(EventWindow {
                bytes_total: 0,
                ..EventWindow::default()
            });
        }
        // One reverse read from EOF: the newest batch, newest-first. Reverse
        // to forward order so a consumer renders top-down.
        self.note_window_read(max_bytes.min(total));
        let rev =
            self.session_log
                .backend()
                .read_lines_reverse(self.session_id, total, max_bytes)?;
        let fwd: Vec<(u64, String)> = rev.lines.into_iter().rev().collect();
        let lines_start_offset = fwd.first().map(|(o, _)| *o).unwrap_or(total);
        let lines_read = fwd.len();
        let (events, skipped) = parse_lines(&fwd);
        Ok(EventWindow {
            events,
            lines_start_offset,
            lines_read,
            // Past EOF == nothing newer; the tail is the newest window.
            next_offset: total,
            bytes_total: total,
            skipped,
        })
    }

    /// The events immediately OLDER than from_byte, in forward order.
    pub(crate) fn window_before(
        &self,
        from_byte: u64,
        max_bytes: u64,
    ) -> Result<EventWindow, ContextError> {
        let total = self.log_size();
        if from_byte == 0 || total == 0 {
            return Ok(EventWindow {
                bytes_total: total,
                ..EventWindow::default()
            });
        }
        self.note_window_read(max_bytes.min(from_byte));
        let rev =
            self.session_log
                .backend()
                .read_lines_reverse(self.session_id, from_byte, max_bytes)?;
        let fwd: Vec<(u64, String)> = rev.lines.into_iter().rev().collect();
        let lines_start_offset = fwd.first().map(|(o, _)| *o).unwrap_or(0);
        let lines_read = fwd.len();
        let (events, skipped) = parse_lines(&fwd);
        Ok(EventWindow {
            events,
            lines_start_offset,
            lines_read,
            next_offset: from_byte,
            bytes_total: total,
            skipped,
        })
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
        self.probe_first_event(anchor.byte_offset)
            .is_some_and(|event| {
                is_user_input(&event.entry) && event.entry.id == anchor.user_input_id
            })
    }

    /// The first complete event at an offset, read without walking the whole
    /// line.
    ///
    /// The record at the offset can be wider than one read step, and a read
    /// whose budget cannot reach a line's start returns nothing, so the step
    /// doubles until one holds a line. The read itself is capped, so a line
    /// wider than that cap cannot be named this way: None means the record at
    /// this offset is unreadable, not that the log holds none.
    fn probe_first_event(&self, offset: u64) -> Option<LocatedEvent> {
        let mut step = PAGE_STEP_BYTES;
        loop {
            let window = self.window(offset, step);
            if let Some(event) = window.events.first() {
                return Some(event.clone());
            }
            if window.lines_read > 0 || step >= MAX_READABLE_LINE_BYTES {
                return None;
            }
            step = step.saturating_mul(2);
        }
    }

    /// One backward batch of a page walk. The step doubles until a read holds
    /// a line, since one event can be wider than a step. Returns the window
    /// and the bytes it spent. A read failure returns an empty batch, not the
    /// log's end.
    fn page_batch(&self, anchor: u64, budget: u64, io_bytes: u64) -> (EventWindow, u64) {
        let mut step = PAGE_STEP_BYTES.min(budget);
        let mut spent = 0u64;
        loop {
            let attempt = step
                .min(anchor)
                .min(PAGE_READ_MAX_BYTES.saturating_sub(io_bytes + spent));
            if attempt == 0 {
                return (EventWindow::default(), spent);
            }
            spent += attempt;
            let window = match self.window_before(anchor, attempt) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!("page reverse-read stopped on failure: {e}");
                    return (EventWindow::default(), spent);
                }
            };
            if window.lines_read > 0 {
                return (window, spent);
            }
            if step >= budget || step >= anchor || io_bytes + spent >= PAGE_READ_MAX_BYTES {
                return (window, spent);
            }
            step = step.saturating_mul(2).min(budget);
        }
    }

    /// The complete turns immediately older than a byte anchor.
    ///
    /// A page counts turns, not bytes: the walk reads back until it has seen
    /// one more turn than it keeps, so the oldest kept turn is whole. When
    /// max_bytes runs out the oldest turn is cut and the page says so. The
    /// walk also stops at the log's start and at a context clear, whose older
    /// turns the session no longer counts.
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
            let (window, spent) = self.page_batch(anchor, budget, io_bytes);
            io_bytes = io_bytes.saturating_add(spent);
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
            let rev = match backend.read_lines_reverse(self.session_id, from, step) {
                Ok(r) => r,
                Err(e) => {
                    // A failure folds the window against what was gathered,
                    // not what the log's end would mean.
                    tracing::warn!("lookback reverse-read stopped on failure: {e}");
                    break;
                }
            };
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

    /// Remember where a history began, once a walk has found it.
    ///
    /// A walk that finds the clear has paid for the answer, so the next reader
    /// of the same history is answered from here instead of walking again. The
    /// entry is a byte offset with the id that makes it meaningful, so a log
    /// rewritten under it is refused rather than read as another history.
    pub(crate) fn remember_epoch_start(&self, epoch: EventId, offset: u64) {
        let Ok(mut scan) = self.scan.lock() else {
            return;
        };
        if scan.epoch_starts.iter().any(|(event, _)| *event == epoch) {
            return;
        }
        scan.epoch_starts.push((epoch, offset));
    }

    /// The byte where a history began, if it is known.
    ///
    /// The event that began an epoch is the newest clear before it, and a clear
    /// is where a walk stops. A start the scan has seen, one a walk has
    /// remembered, or the log's own first byte once its sidecar index confirms
    /// a record is answered from here; the walk is for what none of them knows.
    fn known_epoch_start(&self, epoch: Option<EventId>) -> Option<u64> {
        match epoch {
            // The history the log itself began in starts at the first byte,
            // once the sidecar index says the log holds a record. A log with
            // no index to ask falls through to the head walk, which reads the
            // first event off the log directly.
            None => self
                .session_log
                .backend()
                .log_index_floor(self.session_id, 0)
                .map(|_| 0),
            Some(id) => {
                let scan = self.scan.lock().ok()?;
                scan.epoch_starts
                    .iter()
                    .find(|(event, _)| *event == id)
                    .map(|(_, offset)| *offset)
            }
        }
    }

    /// Run the next chunk of the reverse scan for history epochs. Called per
    /// frame while a full scan is asked for; a no-op once complete. The first
    /// chunk also gives the log's sidecar index its one check and, when the
    /// index is missing or refused, its one streaming rebuild: offset queries
    /// answer from that index, and the rebuild belongs on the worker that
    /// asked for the scan, never on a query path.
    pub(crate) fn index_chunk(&self) -> HistoryIndexProgress {
        let mut scan = self.scan.lock().expect("scan mutex poisoned");
        let backend = self.session_log.backend();
        if scan.total_bytes == 0 {
            scan.total_bytes = backend.log_size(self.session_id);
        }
        if !scan.sidecar_checked {
            scan.sidecar_checked = true;
            if scan.total_bytes > 0
                && backend.log_index_floor(self.session_id, 0).is_none()
                && !backend.rebuild_log_index(self.session_id)
            {
                tracing::warn!(
                    "the log's offset index refused a rebuild, so offset queries answer nothing"
                );
            }
        }
        if scan.done || scan.total_bytes == 0 {
            return HistoryIndexProgress {
                indexed_bytes: scan.built_from_tail,
                total_bytes: scan.total_bytes,
                done: scan.done,
            };
        }
        let from = scan.next_from.unwrap_or(scan.total_bytes);
        let rev = match backend.read_lines_reverse(self.session_id, from, INDEX_CHUNK_BYTES) {
            Ok(r) => r,
            Err(e) => {
                // A stall without advancing: the next call retries.
                tracing::warn!("index reverse-read stalled on failure: {e}");
                return HistoryIndexProgress {
                    indexed_bytes: scan.built_from_tail,
                    total_bytes: scan.total_bytes,
                    done: scan.done,
                };
            }
        };
        // The batch is newest-first, so the scan's epoch table stays that way.
        let batch_epochs = rev.lines.iter().filter_map(|(offset, line)| {
            parse_event(line)
                .filter(|entry| matches!(entry.event, SessionEvent::ContextCleared { .. }))
                .map(|entry| (entry.id, *offset))
        });
        scan.epoch_starts.extend(batch_epochs);
        scan.built_from_tail = scan.total_bytes - rev.next_from.unwrap_or(0);
        scan.next_from = rev.next_from;
        if rev.next_from.is_none() {
            scan.done = true;
        }
        HistoryIndexProgress {
            indexed_bytes: scan.built_from_tail,
            total_bytes: scan.total_bytes,
            done: scan.done,
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
mod tests;
