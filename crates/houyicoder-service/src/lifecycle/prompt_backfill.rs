//! Fill a session's prompt slots from a bounded read of its log.
//!
//! The scan reads a fixed head window and a fixed tail window, so a listing
//! never pays an unbounded read per session, and coverage tells the caller
//! whether the windows met.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use houyicoder_api::session::SessionLog;
use houyicoder_context::DescriptorUpdate;
use houyicoder_context::PromptCoverage;
use houyicoder_context::PromptRecord;
use houyicoder_context::SessionDescriptorError;
use houyicoder_context::SessionDescriptorStore;
use houyicoder_context::SessionId;
use houyicoder_context::SessionLogEntry;
use houyicoder_context::session_intent::apply_prompt_backfill;
use houyicoder_context::session_intent::prompt_intent;
use houyicoder_context::session_intent::stated_input;

/// Bytes read from the head of a log, where the session's opening statement is.
const HEAD_BYTES: u64 = 64 * 1024;

/// Bytes read from the tail of a log, where the newest statement is.
const TAIL_BYTES: u64 = 64 * 1024;

/// What one bounded scan found, and how much of the log it speaks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptScan {
    /// The earliest task statement the windows held.
    pub first: Option<PromptRecord>,
    /// The newest task statement the windows held.
    pub last: Option<PromptRecord>,
    /// How much of the log the two windows account for. Complete means every
    /// byte was read and every record in it parsed.
    pub coverage: PromptCoverage,
    /// Bytes the scan read, so a caller batching many sessions can bound the
    /// work it spends per session.
    pub bytes_read: u64,
}

/// The result of looking for a session's prompts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanOutcome {
    /// The log was read within the byte budget.
    Scanned(PromptScan),
    /// The backend keeps no byte-addressable log, or the session's log holds
    /// no bytes, so nothing can be claimed about the session.
    Unsupported,
    /// The caller asked the scan to stop.
    Cancelled,
}

/// Read the head and tail of a session's log and report the task statements
/// they hold.
///
/// The same judgment the descriptor recorder applies decides which inputs
/// state a task, so a greeting or a frontend command never becomes a prompt
/// here. An interjection counts only when its queue identity says the
/// frontend queued it: the same event shape carries a message another agent
/// sent, and that message states that agent's words rather than the user's.
/// The cancellation flag is read between records, so a scan of a large window
/// stops without waiting for it to finish.
pub fn scan_prompts(
    log: &dyn SessionLog,
    session: SessionId,
    cancelled: &AtomicBool,
) -> ScanOutcome {
    let backend = log.backend();
    if !backend.supports_log_windows() {
        return ScanOutcome::Unsupported;
    }
    let head = backend.read_log_range(session, 0, HEAD_BYTES);
    if head.bytes_total == 0 {
        return ScanOutcome::Unsupported;
    }
    let mut found = Found::default();
    if read_lines(&head.lines, Window::Head, cancelled, &mut found) {
        return ScanOutcome::Cancelled;
    }
    let head_end = head.next_offset;
    let mut coverage = PromptCoverage::Complete;
    if head.unread_records() > 0 || found.malformed {
        coverage = PromptCoverage::Partial;
    }
    let mut bytes_read = head_end;
    if head_end < head.bytes_total {
        // The head stopped short of the end, so the newest statement lives in
        // the tail window.
        match backend.read_lines_reverse(session, head.bytes_total, TAIL_BYTES) {
            Ok(tail) => {
                let tail_start = tail.next_from.unwrap_or(0);
                // Bytes the two reads actually consumed. The windows may
                // overlap, and the bytes in the overlap were read twice, so
                // this counts them twice: it is the disk work the scan did,
                // which is what a caller batching sessions bounds.
                bytes_read = head_end + (head.bytes_total - tail_start);
                if tail_start > head_end || !tail.newest_line_returned {
                    coverage = PromptCoverage::Partial;
                }
                if read_lines(tail.lines.iter().rev(), Window::Tail, cancelled, &mut found) {
                    return ScanOutcome::Cancelled;
                }
                if found.malformed {
                    coverage = PromptCoverage::Partial;
                }
            }
            Err(error) => {
                // The newest statement is unreadable, so only the head may
                // speak and the scan cannot claim the whole log.
                tracing::warn!(
                    session = %session,
                    "prompt backfill: tail read failed: {error}"
                );
                coverage = PromptCoverage::Partial;
            }
        }
    }
    ScanOutcome::Scanned(PromptScan {
        first: found.first,
        last: found.last,
        coverage,
        bytes_read,
    })
}

/// Scan one session's log and fold what it found into the session's record.
/// A cancelled or unsupported scan writes nothing: the record keeps the
/// account it had rather than claiming a coverage the scan did not earn.
pub fn backfill_session(
    log: &dyn SessionLog,
    store: &dyn SessionDescriptorStore,
    session: SessionId,
    cancelled: &AtomicBool,
) -> Result<ScanOutcome, SessionDescriptorError> {
    let outcome = scan_prompts(log, session, cancelled);
    if let ScanOutcome::Scanned(scan) = &outcome {
        let update = store.update_descriptor(session, &mut |descriptor| {
            apply_prompt_backfill(
                descriptor,
                scan.first.clone(),
                scan.last.clone(),
                scan.coverage,
            );
        })?;
        if update == DescriptorUpdate::Absent {
            // Nothing on disk holds this session's record, and the scan cannot
            // create one. The walk moves past it rather than stalling on a
            // session no pass can repair; a record that reappears is scanned
            // again when the walk wraps.
            tracing::warn!(
                session = %session,
                "prompt backfill: no record to fill"
            );
        }
    }
    Ok(outcome)
}

/// Which end of the log a window reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Window {
    /// The window that starts at byte zero.
    Head,
    /// The window that ends at the log's last byte.
    Tail,
}

/// The task statements one pass over a window found, kept as the earliest and
/// newest by event id rather than by arrival: the two windows meet in no
/// guaranteed order.
#[derive(Default)]
struct Found {
    first: Option<PromptRecord>,
    last: Option<PromptRecord>,
    /// A record the window reached but could not parse. The scan then cannot
    /// say it saw every event, whatever the byte coverage.
    malformed: bool,
}

impl Found {
    /// Fold a hit from one window into the slots that window may fill. The
    /// head may fill both: it begins at the log's first byte, so its earliest
    /// hit is the earliest statement the scan could have read. The tail may
    /// fill only the newest slot; a statement outside the tail was merely not
    /// read, and an older session must not gain a first prompt the scan never
    /// saw.
    fn fold(&mut self, window: Window, record: PromptRecord) {
        if window == Window::Head
            && self
                .first
                .as_ref()
                .is_none_or(|first| record.event_id < first.event_id)
        {
            self.first = Some(record.clone());
        }
        if self
            .last
            .as_ref()
            .is_none_or(|last| last.event_id < record.event_id)
        {
            self.last = Some(record);
        }
    }
}

/// Read one window's complete lines. True when the caller asked to stop.
fn read_lines<'a>(
    lines: impl IntoIterator<Item = &'a (u64, String)>,
    window: Window,
    cancelled: &AtomicBool,
    found: &mut Found,
) -> bool {
    for (_, line) in lines {
        if cancelled.load(Ordering::Relaxed) {
            return true;
        }
        let Ok(entry) = serde_json::from_str::<SessionLogEntry>(line) else {
            found.malformed = true;
            continue;
        };
        let Some(text) = stated_input(&entry.event) else {
            continue;
        };
        if let Some(text) = prompt_intent(text) {
            let record = PromptRecord {
                text,
                event_id: entry.id,
            };
            found.fold(window, record);
        }
    }
    false
}

#[cfg(test)]
mod tests;
