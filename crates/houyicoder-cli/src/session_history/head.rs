//! The forward read that finds where the history in hand starts.
//!
//! The page reads walk backwards from the end of the log, so the oldest turns
//! are the one place they cannot reach without reading everything. This module
//! answers that from the other end, and it is the only reader that has to know
//! which event began the history: a cleared session counts what came after the
//! clear, so its head is not the log's first turn.

use houyicoder_context::EventId;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    EventWindow, INDEX_CHUNK_BYTES, LocatedEvent, MAX_READABLE_LINE_BYTES, PAGE_STEP_BYTES,
    SessionHistory, TurnPage, is_user_input, parse_lines,
};

impl SessionHistory {
    /// The oldest complete turns of the history that began at epoch.
    ///
    /// The head is the one page a backwards walk cannot reach without reading
    /// the whole log, so it is read from the other end: walk forward until the
    /// page holds one turn more than it keeps, then cut at the opening of the
    /// oldest turn kept. The page never starts mid-turn, so it carries no
    /// partial flag; a log whose first event is not a user input still opens a
    /// turn there, which the projection numbers as the session's first.
    ///
    /// A cleared session counts only what came after the clear, so the page
    /// starts at the event that began the epoch rather than at the log's first
    /// event. An empty page means the epoch could not be located, which a
    /// caller reads as nothing to show rather than as the log's start.
    pub(crate) fn head_turns(
        &self,
        epoch: Option<EventId>,
        page_turns: usize,
        max_bytes: u64,
        cancel: &AtomicBool,
    ) -> TurnPage {
        let total = self.log_size();
        if total == 0 || page_turns == 0 {
            return TurnPage::default();
        }
        let Some(start) = self.epoch_start_offset(epoch, cancel) else {
            return TurnPage::default();
        };
        let mut anchor = start;
        let mut budget = max_bytes;
        let mut collected: Vec<LocatedEvent> = Vec::new();
        let mut opened = 0usize;
        let mut skipped = 0usize;
        // One more turn than the page keeps, so the opening of the oldest kept
        // turn is in hand and the page can start there.
        let wanted = page_turns + 1;
        while opened < wanted && anchor < total && budget > 0 {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            // A single event can be wider than one step, and a read whose
            // budget cannot reach a line's start returns nothing, so the step
            // doubles until it holds a line or the budget is spent. Without
            // this the page would come back empty on a log whose first event
            // is wide.
            let mut step = PAGE_STEP_BYTES.min(budget);
            let window = loop {
                let attempt = step.min(total - anchor);
                if attempt == 0 {
                    break EventWindow::default();
                }
                let window = self.window(anchor, attempt);
                if window.lines_read > 0 {
                    break window;
                }
                if step >= budget || step >= total - anchor {
                    break window;
                }
                step = step.saturating_mul(2).min(budget);
            };
            if window.lines_read == 0 || window.next_offset <= anchor {
                break;
            }
            budget = budget.saturating_sub(window.next_offset - anchor);
            skipped += window.skipped;
            opened += window
                .events
                .iter()
                .filter(|event| is_user_input(&event.entry))
                .count();
            collected.extend(window.events);
            anchor = window.next_offset;
        }
        // The page keeps the FIRST turns of the history, so it is cut where the
        // turn after them opens rather than where the newest ones begin.
        if let Some(cut) = collected
            .iter()
            .enumerate()
            .filter(|(_, event)| is_user_input(&event.entry))
            .map(|(i, _)| i)
            .nth(page_turns)
        {
            collected.truncate(cut);
        }
        TurnPage {
            events: collected,
            // The head is where the history begins, so there is nothing older
            // to anchor a walk behind it.
            oldest_anchor: None,
            oldest_partial: false,
            skipped,
            // A head page is not where the log ends, so it never anchors an
            // append: only the window that follows the tail does.
            end_offset: 0,
        }
    }

    /// The byte offset where the history that began at epoch starts.
    ///
    /// The common case costs one line: a history never cleared begins at the
    /// log's first event, so the epoch and that event are the same. A cleared
    /// history began somewhere behind, and the log is walked back to the event
    /// that began it.
    ///
    /// That walk is chunked, so it holds one chunk of the log at a time
    /// however far back the clear sits, and it runs on the reader's worker
    /// rather than on a draw. Every chunk is counted as a read, so a head that
    /// had to search is visible in the reader's byte budget instead of
    /// costing more than the budget claims. It also watches the read's cancel
    /// flag: a walk nobody waits for stops at the next chunk rather than
    /// reading a whole long log for a page that will be dropped.
    fn epoch_start_offset(&self, epoch: Option<EventId>, cancel: &AtomicBool) -> Option<u64> {
        let first = self.first_event()?;
        if epoch.is_none_or(|id| id == first.entry.id) {
            return Some(first.byte_offset);
        }
        let mut from = self.log_size();
        while from > 0 {
            if cancel.load(Ordering::Acquire) {
                return None;
            }
            let step = INDEX_CHUNK_BYTES.min(from);
            self.note_window_read(step);
            let rev = self
                .session_log
                .backend()
                .read_lines_reverse(self.session_id, from, step);
            let fwd: Vec<(u64, String)> = rev.lines.iter().rev().cloned().collect();
            let (events, _) = parse_lines(&fwd);
            if let Some(found) = events
                .iter()
                .find(|event| Some(event.entry.id) == epoch)
                .map(|event| event.byte_offset)
            {
                return Some(found);
            }
            match rev.next_from {
                Some(next) if next < from => from = next,
                _ => break,
            }
        }
        None
    }

    /// The log's first complete event, read without walking the whole line.
    ///
    /// A first line wider than one read step yields no complete line, so the
    /// step doubles until it holds one. The read itself is capped, so a line
    /// wider than that cap cannot be named this way: None means the first
    /// event is unreadable, not that the log is empty.
    fn first_event(&self) -> Option<LocatedEvent> {
        let mut step = PAGE_STEP_BYTES;
        loop {
            let window = self.window(0, step);
            if let Some(event) = window.events.first() {
                return Some(event.clone());
            }
            if window.lines_read > 0 || step >= MAX_READABLE_LINE_BYTES {
                return None;
            }
            step = step.saturating_mul(2);
        }
    }
}
