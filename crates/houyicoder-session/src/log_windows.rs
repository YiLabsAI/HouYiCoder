//! Reading a session's durable log through forward line-aligned byte windows.
//!
//! Three readers share the window mechanics. One folds a reopened session's
//! whole epoch into the rows the log state keeps, which is how a resumed session
//! becomes readable without the process ever holding the history. One reads a
//! page of records older than the log state's window, which is how a reader walks
//! a history larger than memory. One hashes a log's lines to verify the links
//! between them, and reads the same windows so a writer and a verifier cannot
//! disagree about which bytes hold a record.
//!
//! The two ordinal readers count the same way the log state does: an ordinal per
//! record from the current epoch's clear boundary. A window widens to hold a
//! record that overruns it, so a record is read whole; one wider than the
//! backend's ceiling is beyond any read, and both count its ordinal and report
//! its bytes, so a count that lost records is not read as a history that held
//! none.
//!
//! The parse is strict, matching the whole-log read: a corrupt line is a
//! corrupt session, not a gap to skip silently.

use houyicoder_api::session::{TrajectoryPage, TrajectoryPosition};
use houyicoder_context::{
    ContextBackend, ContextError, LogRangeRead, MAX_WINDOW_BYTES, SessionEvent, SessionId,
    SessionLogEntry,
};

use crate::trajectory::RestoredHistory;

/// The bytes one window read asks for, before widening. A caller asking for
/// more than the backend's own ceiling gets the ceiling, so this states the
/// window the folds and walks are built around.
const WINDOW_BYTES: u64 = 1 << 20;

/// Read one window of a session's log, widening it to hold a line.
///
/// A record whose line is wider than the window comes back as a window with no
/// line, and the record cannot be read in parts: widening asks again from the
/// same offset with twice the bytes, so the record is read whole rather than
/// passed over. Widening stops at the backend's ceiling and at the log's end,
/// so a record wider than any read, or an unterminated tail, costs one window
/// and no more; the terminator count is what tells the caller the difference.
pub(crate) fn read_window(
    backend: &dyn ContextBackend,
    session: SessionId,
    offset: u64,
    want: u64,
) -> LogRangeRead {
    let mut want = want;
    loop {
        let window = backend.read_log_range(session, offset, want);
        let at_end = window.next_offset >= window.bytes_total;
        if !window.lines.is_empty() || at_end || want >= MAX_WINDOW_BYTES {
            return window;
        }
        want = (want * 2).min(MAX_WINDOW_BYTES);
    }
}

/// Fold a session's durable log into the rows the log state keeps.
///
/// One forward pass over the log's windows, bounded by the resident budget: the
/// folded history holds its newest rows and the counts for all of them. Each
/// clear boundary starts the fold over, so the history is the epoch the session
/// currently counts rather than the sum of every epoch the log holds.
pub(crate) fn fold_epoch(
    backend: &dyn ContextBackend,
    session: SessionId,
    budget_bytes: usize,
) -> Result<RestoredHistory, ContextError> {
    let held = backend.log_size(session);
    let mut offset = 0u64;
    // The epoch begins where the fold begins reading: the log's head. A clear
    // boundary read later moves it to the boundary record, which opens the
    // epoch that survives the read.
    let mut history = RestoredHistory::new(budget_bytes, Some(offset));
    loop {
        let window = read_window(backend, session, offset, WINDOW_BYTES);
        if window.bytes_total == 0 && held > 0 {
            // The log holds bytes and this read reached none of them: the
            // backend failed, and folding on would report a session with no
            // records as the session's content.
            return Err(ContextError::Io);
        }
        let consumed: u64 = window
            .lines
            .iter()
            .map(|(_, line)| line.len() as u64 + 1)
            .sum();
        let passed = window.next_offset.saturating_sub(offset + consumed);
        // The record the read passed over stands before the lines it holds, so
        // its ordinal is counted first. Only a record past the ceiling is passed
        // over at all, and the read that reaches its terminator is the one that
        // reports it, so the record takes one ordinal.
        for _ in 0..window.unread_records() {
            history.count_unread();
        }
        for (line_offset, line) in &window.lines {
            if line.is_empty() {
                continue;
            }
            let entry = parse_record(line)?;
            if matches!(entry.event, SessionEvent::ContextCleared { .. }) {
                history.restart_epoch(*line_offset);
            }
            history.push(entry);
        }
        if passed > 0 {
            // Bytes of a record the read reached without holding: the rest of
            // a line wider than a window, before the lines or after them. The
            // fold keeps no row for those bytes, so it reports them.
            history.note_skipped(passed);
        }
        if window.next_offset <= offset {
            break;
        }
        offset = window.next_offset;
    }
    Ok(history)
}

/// Read one page of durable records from the log, for a position the log state's
/// window does not cover.
///
/// A position carrying a byte offset resumes exactly there. One carrying none
/// starts where the epoch begins and counts its way to the ordinal, which costs
/// one parse per record passed.
///
/// The walk stops at the horizon -- the ordinal where the resident window takes
/// over -- so a page never repeats a record the window holds.
pub(crate) fn read_page(
    backend: &dyn ContextBackend,
    session: SessionId,
    from: TrajectoryPosition,
    limit: usize,
    horizon: usize,
    epoch_byte_offset: Option<u64>,
) -> Result<TrajectoryPage, ContextError> {
    let held = backend.log_size(session);
    let (mut offset, mut ordinal) = match (from.byte_offset, epoch_byte_offset) {
        (Some(offset), _) => (offset, from.index),
        (None, Some(epoch)) => (epoch, 0),
        (None, None) => return Ok(unreadable_position(from)),
    };
    let mut events = Vec::new();
    let mut skipped_bytes = 0u64;
    loop {
        if ordinal >= horizon || events.len() >= limit {
            return Ok(page(events, ordinal, offset, skipped_bytes));
        }
        let window = read_window(backend, session, offset, WINDOW_BYTES);
        if window.bytes_total == 0 && held > 0 {
            return Err(ContextError::Io);
        }
        let consumed: u64 = window
            .lines
            .iter()
            .map(|(_, line)| line.len() as u64 + 1)
            .sum();
        let passed = window.next_offset.saturating_sub(offset + consumed);
        skipped_bytes = skipped_bytes.saturating_add(passed);
        if window.lines.is_empty() && window.next_offset <= offset {
            // The log ends before the horizon, so the records the log state
            // counts above this point are not on disk to read. The walk
            // reports where it stopped rather than claiming the rest.
            return Ok(page(events, ordinal, offset, skipped_bytes));
        }
        // A record the read passed over takes its ordinal before the lines the
        // same read holds, because its bytes stand before them. Only a record
        // past the ceiling is passed over at all, and the read that reaches its
        // terminator is the one that reports it, so it is counted once.
        for _ in 0..window.unread_records() {
            ordinal += 1;
        }
        if ordinal >= horizon {
            // The horizon is the first ordinal the window holds, so the walk
            // stops here. The position it reports is where the first line of
            // this read begins, which is the record that ordinal names.
            let next = window
                .lines
                .first()
                .map_or(window.next_offset, |(start, _)| *start);
            return Ok(page(events, ordinal, next, skipped_bytes));
        }
        for (line_offset, line) in &window.lines {
            if line.is_empty() {
                continue;
            }
            if ordinal >= from.index && events.len() < limit {
                events.push(parse_record(line)?);
            }
            ordinal += 1;
            offset = *line_offset + line.len() as u64 + 1;
            if ordinal >= horizon || events.len() >= limit {
                return Ok(page(events, ordinal, offset, skipped_bytes));
            }
        }
        offset = window.next_offset;
    }
}

/// The page a walk reports: the records it read, the ordinal and byte position
/// the next read continues from, and the bytes it passed over unread.
fn page(
    events: Vec<SessionLogEntry>,
    index: usize,
    byte_offset: u64,
    skipped_bytes: u64,
) -> TrajectoryPage {
    TrajectoryPage {
        events,
        next: TrajectoryPosition {
            index,
            byte_offset: Some(byte_offset),
        },
        skipped_bytes,
    }
}

/// The page for a position nothing can resolve: the log state kept no byte
/// position for its epoch and the caller named no byte offset of its own.
fn unreadable_position(from: TrajectoryPosition) -> TrajectoryPage {
    TrajectoryPage {
        next: from,
        ..TrajectoryPage::default()
    }
}

/// Parse one record. Strict, matching the whole-log read: a line that does not
/// parse is a corrupt session rather than a gap to step over.
fn parse_record(line: &str) -> Result<SessionLogEntry, ContextError> {
    serde_json::from_str(line).map_err(|e| ContextError::Corrupt(format!("bad event line: {e}")))
}
