//! The offset queries answered from the sidecar index a log carries beside it.
//!
//! The index samples records by byte interval, so a query walks forward from
//! the entry covering its target: one interval of distance at most, bounded
//! here in case the index is stale or the records are wide. Every answer is
//! checked against the log's bytes before it is returned, so an index that
//! does not hold degrades a query to None instead of to a wrong offset.

use houyicoder_context::LogIndexEntry;

use super::{MAX_READABLE_LINE_BYTES, SessionHistory};

/// The most bytes one query walks forward from its index entry. The sampling
/// interval is a quarter mebibyte, so a target between two entries sits within
/// one interval of the entry covering it; the ceiling leaves room for wide
/// records and for a log tail grown past the last sample, and it bounds what a
/// query against a stale index costs before it gives up.
pub(crate) const INDEX_WALK_MAX_BYTES: u64 = 8 * 1024 * 1024;

impl SessionHistory {
    /// The byte offset of the record at a position, counting parsed records
    /// from the log's start. None when the log's sidecar index is missing or
    /// refused, when the covering entry fails its check against the log, or
    /// when the record sits further from that entry than the walk budget
    /// reaches.
    pub(crate) fn byte_at(&self, event_idx: usize) -> Option<u64> {
        let ordinal = u64::try_from(event_idx).ok()?;
        let entry = self.sidecar_entry(ordinal)?;
        let ahead = ordinal.checked_sub(entry.ordinal)?;
        if !self.entry_holds(entry) {
            return None;
        }
        if ahead == 0 {
            return Some(entry.offset);
        }
        let mut cursor = entry.offset;
        let mut walked = 0u64;
        let mut advanced = 0u64;
        while walked < INDEX_WALK_MAX_BYTES {
            let step = (INDEX_WALK_MAX_BYTES - walked).min(MAX_READABLE_LINE_BYTES);
            let window = self.window(cursor, step);
            // A read that reached the log's end holds no line, and neither
            // does one whose budget died inside a single line. The first means
            // the index named a record the log does not hold; the second hides
            // records the walk cannot count. Neither has an answer.
            if window.lines_read == 0 {
                return None;
            }
            walked += window.next_offset - cursor;
            // The first parsed record of the first window is the entry's own,
            // which the check above confirmed, so the walk starts past it.
            let start = if cursor == entry.offset { 1 } else { 0 };
            for event in window.events.iter().skip(start) {
                advanced += 1;
                if advanced == ahead {
                    return Some(event.byte_offset);
                }
            }
            cursor = window.next_offset;
        }
        None
    }

    /// The number of parsed records the log holds. None under the same
    /// conditions as byte_at, and when the walk from the index's last entry to
    /// the log's end outruns the budget: an index that far behind the log says
    /// nothing about its length, and a wrong count is worse than none.
    pub(crate) fn event_count(&self) -> Option<usize> {
        let entry = self.sidecar_entry(u64::MAX)?;
        if !self.entry_holds(entry) {
            return None;
        }
        let mut cursor = entry.offset;
        let mut walked = 0u64;
        // The entry's ordinal counts the records before it; the record it
        // names is the one the walk starts on.
        let mut records = entry.ordinal.checked_add(1)?;
        loop {
            let step = (INDEX_WALK_MAX_BYTES - walked).min(MAX_READABLE_LINE_BYTES);
            if step == 0 {
                // The walk budget is spent. The count is whole only if the
                // cursor has reached the log's end; bytes left unread past it
                // hide records a stopped count would miss.
                if cursor >= self.log_size() {
                    break;
                }
                return None;
            }
            let window = self.window(cursor, step);
            if window.next_offset <= cursor {
                if window.bytes_total > 0 && cursor >= window.bytes_total {
                    // The log ends at the cursor: every record has been
                    // counted.
                    break;
                }
                // The read stopped short of the log's end -- a failed open, a
                // failed seek, or a line too wide for the budget. The records
                // between the cursor and the end cannot be counted, and a
                // count that stops early is a wrong count.
                return None;
            }
            if window.lines_read == 0 {
                // A single line wider than a read: the walk cannot count the
                // records past it, so the count cannot finish.
                return None;
            }
            walked += window.next_offset - cursor;
            let start = if cursor == entry.offset { 1 } else { 0 };
            records += u64::try_from(window.events.len().saturating_sub(start)).ok()?;
            cursor = window.next_offset;
        }
        usize::try_from(records).ok()
    }

    /// The index entry covering an ordinal, asked of the backend that holds
    /// the log. The backend checks the file against the log on every open --
    /// a stat, the log's first record identity, the index header, and a
    /// logarithmic count of entry seeks -- at a cost bounded by the index
    /// itself rather than by any window this module walks, so an entry
    /// arriving here already belongs to this log's history.
    fn sidecar_entry(&self, ordinal: u64) -> Option<LogIndexEntry> {
        self.session_log
            .backend()
            .log_index_floor(self.session_id, ordinal)
    }

    /// Whether the bytes at an index entry's offset still hold the record the
    /// entry names.
    ///
    /// The index is derived data, written without a sync and rebuildable at
    /// any time, while the log is the truth. An entry that fails this check
    /// was built over bytes that are no longer there, and every offset walked
    /// from it would name the wrong record.
    fn entry_holds(&self, entry: LogIndexEntry) -> bool {
        self.probe_first_event(entry.offset)
            .is_some_and(|event| event.byte_offset == entry.offset && event.entry.id == entry.event)
    }
}
