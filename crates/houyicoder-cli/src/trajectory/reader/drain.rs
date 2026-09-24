//! Applying a finished page read to the window.
//!
//! A read is validated before anything resident changes: a page that does not
//! describe the history its watermark claims, or a delta that stopped short of
//! the byte it was dispatched for, is refused and read again rather than
//! applied as a window the session does not have.

use super::{
    DurableWatermark, PageRead, READ_FAILURES_BEFORE_FAILED, RESIDENT_PAGES, SessionLogTrajectory,
    TrajectoryState, TurnPage,
};

impl SessionLogTrajectory {
    /// Whether a page's last event is the one its read was dispatched for.
    ///
    /// A read can come back short: the log's last line may be a partial write,
    /// or a rewrite may have moved the bytes. The page then describes less than
    /// the watermark it would be applied under, so it is refused rather than
    /// applied as history the window does not hold.
    pub(super) fn reaches_watermark(page: &TurnPage, dispatched: DurableWatermark) -> bool {
        match dispatched.last_id {
            Some(id) => page.events.last().is_some_and(|event| event.entry.id == id),
            None => page.events.is_empty(),
        }
    }

    /// Apply a page to the window, for the read it was dispatched for.
    ///
    /// The read decides what the page means: an older page is prepended, the
    /// head replaces the window, a delta extends the page it was read behind,
    /// and a tail page replaces the window unless the user walked away from it.
    ///
    /// True when the page was applied. A page the window could not take -- a
    /// delta behind another page, an anchor that no longer holds, a tail the
    /// user walked away from -- leaves the window as it was, and the caller
    /// must not stamp the watermark it was read for: that would claim the
    /// window holds a history it never took.
    pub(super) fn apply_page(
        state: &mut TrajectoryState,
        read: PageRead,
        outcome: TurnPage,
        dispatched: DurableWatermark,
    ) -> bool {
        match read {
            // An older page is anchored to a durable turn, so a later
            // append does not invalidate it: it still abuts the page it
            // was read behind. The window slides rather than grows, so
            // the page furthest from the walk, the newest one, is the
            // one dropped.
            PageRead::Older(_) => {
                let arrived = outcome;
                if arrived.events.is_empty() {
                    // The anchor no longer names the turn it did, so
                    // nothing resident can be trusted to abut the log:
                    // the window is dropped and the tail read again
                    // rather than paging the wrong history.
                    state.pages.clear();
                    state.follow_tail = true;
                    state.older_hidden = 0;
                    state.window_watermark = None;
                    Self::drop_view(state);
                    return false;
                }
                let reached_start = arrived.oldest_anchor.is_none();
                let arrived_turns = arrived.turn_count();
                state.pages.push_front(arrived);
                if reached_start {
                    // The walk reached the log's first turn, so nothing
                    // sits before the window however the counts read.
                    state.older_hidden = 0;
                } else {
                    state.older_hidden = state.older_hidden.saturating_sub(arrived_turns);
                }
                if state.pages.len() > RESIDENT_PAGES {
                    state.pages.pop_back();
                    state.follow_tail = false;
                }
            }
            // The head is the other end of the log, so the window
            // becomes it and nothing sits before it.
            PageRead::Head => {
                if outcome.events.is_empty() {
                    // The history's start could not be located, so
                    // there is no head to show. A window in hand is
                    // still valid and stays where it is; an empty one
                    // falls back to the tail, which is the only end
                    // left to read.
                    if state.pages.is_empty() {
                        state.follow_tail = true;
                    }
                    Self::drop_view(state);
                    return false;
                }
                state.pages.clear();
                state.pages.push_back(outcome);
                state.older_hidden = 0;
                state.follow_tail = false;
            }
            // A delta describes what the log appended after the byte
            // the window ends at. A read that starts anywhere else is
            // not that, so it is dropped rather than spliced into the
            // wrong place.
            PageRead::Append { from, to } => {
                if state
                    .pages
                    .back()
                    .is_none_or(|page| page.end_offset != from)
                {
                    Self::drop_view(state);
                    return false;
                }
                // The delta is applied whole or not at all: a delta
                // that stopped short of the byte it was dispatched for
                // would leave the page holding bytes its own watermark
                // does not describe, so it is refused and read again.
                if outcome.end_offset < to {
                    Self::refuse(state, dispatched);
                    return false;
                }
                if let Some(back) = state.pages.back_mut() {
                    back.end_offset = outcome.end_offset;
                    back.skipped += outcome.skipped;
                    back.events.extend(outcome.events);
                }
            }
            PageRead::Tail { .. } => {
                // A window the user walked away from is theirs, so a
                // tail page read before that does not move it. An empty
                // window has nothing to protect: it takes the tail and
                // follows it, which is the only way it leaves loading.
                if !state.follow_tail && !state.pages.is_empty() {
                    Self::drop_view(state);
                    return false;
                }
                state.follow_tail = true;
                state.pages.clear();
                state.pages.push_back(outcome);
            }
        }
        true
    }

    /// Refuse a read that cannot be applied, the way a dead worker is refused:
    /// a run of them is reported rather than dispatching a worker per frame.
    fn refuse(state: &mut TrajectoryState, dispatched: DurableWatermark) {
        state.read_failures += 1;
        if state.read_failures >= READ_FAILURES_BEFORE_FAILED {
            state.failed = true;
            state.failed_watermark = Some(dispatched);
        }
        Self::drop_view(state);
    }

    /// Take a finished page, if one is ready. Called from a draw, so it never
    /// blocks.
    pub(super) fn drain(&self, state: &mut TrajectoryState, current: DurableWatermark) {
        let Some(pending) = state.pending.as_ref() else {
            return;
        };
        match pending.rx.try_recv() {
            Ok(outcome) => {
                let read = pending.read;
                let dispatched = pending.dispatched;
                let total_turns = pending.total_turns;
                state.pending = None;
                if dispatched.epoch != current.epoch {
                    // The session was cleared while this read was in flight.
                    Self::drop_view(state);
                    return;
                }
                // A read that claims the end of the history must have reached
                // it: a short page describes less than the watermark it would
                // be applied under. The check comes before anything resident
                // changes, so a refused read leaves the window as it was.
                let claims_end = matches!(read, PageRead::Tail { .. } | PageRead::Append { .. });
                if claims_end && !Self::reaches_watermark(&outcome, dispatched) {
                    Self::refuse(state, dispatched);
                    return;
                }
                // Only an applied page clears the run: a refused one is what
                // the run is counting.
                state.failed = false;
                state.read_failures = 0;
                if Self::apply_page(state, read, outcome, dispatched) {
                    state.window_watermark = Some(dispatched);
                    state.window_total = total_turns;
                }
                Self::drop_view(state);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // The worker went away without sending. One such read is
                // retried rather than reported: a transient failure must not
                // leave the pane unusable for the rest of the session. A run of
                // them is reported, so a broken read does not dispatch a worker
                // on every frame either.
                let dispatched = pending.dispatched;
                state.pending = None;
                state.read_failures += 1;
                state.window_watermark = None;
                // A read that never arrived leaves no view to serve: dropping
                // it is what lets the next frame dispatch the retry.
                Self::drop_view(state);
                if state.read_failures >= READ_FAILURES_BEFORE_FAILED {
                    state.failed = true;
                    state.failed_watermark = Some(dispatched);
                }
            }
        }
    }
}
