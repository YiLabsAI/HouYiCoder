//! The trajectory pane's page reader.
//!
//! The turn list comes from a bounded page of the durable log, read by a
//! worker thread and projected into the view the pane draws. The reader itself
//! never touches the disk on a draw: it serves the window it holds and reports
//! a read in flight as loading. This module owns the read state and when a read
//! is needed; the view the pane draws is built in the window view submodule.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use houyicoder_api::session::{SessionLog, TrajectoryHead};
use houyicoder_context::{EventId, SessionId};
use houyicoder_tui::view::trajectory_pane::{TrajectoryLog, TrajectoryView, TrajectoryViewState};

use crate::session_history::{PAGE_MAX_BYTES, SessionHistory, TurnAnchor, TurnPage};

mod window_view;

use crate::trajectory_view::project;

/// How many turns the pane loads by default, and how many it adds each time
/// the user asks for older history.
pub(crate) const TRAJECTORY_PAGE_TURNS: usize = 100;

/// How many pages stay resident.
///
/// The window slides rather than growing: the user walks backwards, so the
/// page furthest from the walk is the newest one, and it is the one dropped.
/// Rows keep their session turn number across the drop, so a caller holding
/// one can still find the turn it named.
pub(crate) const RESIDENT_PAGES: usize = 2;

/// How many reads may end without a page before the pane reports failure
/// rather than retrying. One is a transient; a run of them is a broken read,
/// and retrying it every frame would spawn a worker per frame.
const READ_FAILURES_BEFORE_FAILED: usize = 3;

/// Which durable history a page was read for.
///
/// The count alone is not enough: a cleared session starts a new history, and
/// its first events can reach the same count the old one had. The newest
/// durable id separates the two.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DurableWatermark {
    /// The event that began the epoch the page was read in. Two watermarks
    /// with different epochs describe different sessions, however their counts
    /// compare: a clear starts a new one, and a page from the old one must
    /// never be shown.
    epoch: Option<EventId>,
    count: usize,
    last_id: Option<EventId>,
}

/// Which end of the log a page read starts from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PageRead {
    /// The newest turns, read backwards from EOF.
    Tail,
    /// The complete turns older than a durable anchor.
    Older(TurnAnchor),
    /// The oldest turns, read forward from the start of the log.
    Head,
}

/// A page read in flight, and what it was dispatched for.
struct PendingPageRead {
    /// The durable history this read was dispatched for.
    dispatched: DurableWatermark,
    /// The session's turn total when the read was dispatched, which is what
    /// the page it returns is numbered from.
    total_turns: usize,
    read: PageRead,
    rx: std::sync::mpsc::Receiver<TurnPage>,
    /// Set when the read is superseded, so a read with more to do than one
    /// page stops instead of finishing for nobody. A head read walks the log
    /// to find where the history began, and a superseded one that kept walking
    /// would hold the disk and the history reader for as long as that takes.
    cancel: Arc<AtomicBool>,
}

/// The pane's read state: the resident pages, the view projected from them, and
/// the read in flight, if any.
///
/// The pane draws every frame, so nothing here reads the log. A page arrives
/// from a worker and is applied on a later frame; until it does, the view
/// reports that it is loading rather than showing an empty list.
#[derive(Default)]
struct TrajectoryState {
    /// Oldest first: the resident pages in log order. The window is their
    /// concatenation, so a page can be dropped from either end without moving
    /// the rows the other end holds.
    pages: VecDeque<TurnPage>,
    /// Session turn numbers that sit before the window's first turn. Derived
    /// while the window ends at the tail; frozen once the user walks away from
    /// it and moved by each page that arrives.
    older_hidden: usize,
    /// Whether the window ends at the session's newest turn, so an append
    /// refreshes it. False once the user walks back or jumps to the head: the
    /// window is theirs then, and an append must not move it under them.
    follow_tail: bool,
    /// The epoch the resident pages were read in, and how many histories this
    /// reader has seen. A clear starts a new history whose turn numbers begin
    /// again, so the count is what a view can compare to tell whether a turn
    /// number still names the same turn.
    seen_epoch: Option<EventId>,
    history_generation: u64,
    view: Option<Arc<TrajectoryView>>,
    /// The state the cached view was built for, so a frame while a read is in
    /// flight serves it instead of rebuilding the window.
    view_state: Option<TrajectoryViewState>,
    /// The durable history the resident pages belong to. A clear starts a new
    /// epoch, and a page read in the old one describes turns the session no
    /// longer counts, so this is what decides whether the window is still the
    /// session's.
    window_watermark: Option<DurableWatermark>,
    /// The session's turn total as of the resident window's last read.
    ///
    /// A row keeps the number it had when its page was read, so the total the
    /// window is numbered from has to be the one that was in hand then. The
    /// session's own total moves on every append, and numbering a page that is
    /// still in hand from it would rename every row in the window.
    window_total: usize,
    /// The durable history the cached view was built for. A streaming delta
    /// moves the mirror revision but not this, so keying on it is what keeps a
    /// stream from re-reading the log once per frame.
    ///
    /// The consequence, deliberate: the header's figures refresh at durable
    /// event boundaries rather than per streamed token, so the session's
    /// elapsed seconds hold steady while a model is mid-answer. The
    /// alternative is rebuilding the view on every token, which is a page fold
    /// and a set of allocations on the draw path.
    view_watermark: Option<DurableWatermark>,
    /// How many turns the cached view was built for, so asking for more
    /// rebuilds it rather than serving the narrower one.
    max_turns: usize,
    pending: Option<PendingPageRead>,
    /// Consecutive reads that ended without a page. One is retried; a run of
    /// them means the reads are not working, so the pane says so instead of
    /// dispatching a worker per frame forever.
    read_failures: usize,
    failed: bool,
    /// The history the failure belongs to. A failure is a fact about one read,
    /// not about the session: when the durable history moves on, the pane tries
    /// again instead of staying failed for the rest of the session.
    failed_watermark: Option<DurableWatermark>,
}

/// Session log trajectory reader.
///
/// The turn list comes from a bounded page of the durable log, read by a worker
/// thread and projected into the view the pane draws. The reader itself never
/// touches the disk: a draw serves the page it holds and reports a read in
/// flight as loading.
pub struct SessionLogTrajectory {
    pub(crate) session_log: Arc<dyn SessionLog>,
    pub(crate) session_id: SessionId,
    pub(crate) model: String,
    history: Arc<SessionHistory>,
    state: Mutex<TrajectoryState>,
    /// Turns to load: one page at first, grown when the user walks past the
    /// oldest loaded turn.
    loaded_turns: AtomicUsize,
}

impl SessionLogTrajectory {
    /// Build a reader over a history reader the caller already owns, so the
    /// transcript snapshot and this view share one set of byte windows and one
    /// offset index.
    pub fn with_history(
        history: Arc<SessionHistory>,
        session_log: Arc<dyn SessionLog>,
        session_id: SessionId,
        model: String,
    ) -> Self {
        Self {
            session_log,
            session_id,
            model,
            history,
            // The pane opens at the tail, so the first read follows it.
            state: Mutex::new(TrajectoryState {
                follow_tail: true,
                ..TrajectoryState::default()
            }),
            loaded_turns: AtomicUsize::new(TRAJECTORY_PAGE_TURNS),
        }
    }

    fn max_turns(&self) -> usize {
        self.loaded_turns.load(Ordering::Relaxed)
    }

    /// Start a page read on a worker thread. The page is bounded, so the worker
    /// finishes on its own; a reader that is dropped first simply leaves its
    /// result unclaimed.
    fn dispatch(
        &self,
        state: &mut TrajectoryState,
        read: PageRead,
        dispatched: DurableWatermark,
        total_turns: usize,
    ) {
        let history = self.history.clone();
        let page_turns = self.max_turns();
        let epoch = dispatched.epoch;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let page = match read {
                PageRead::Tail => history.tail_turns(page_turns, PAGE_MAX_BYTES),
                // The offset alone cannot say whether it still names the turn
                // it did when the anchor was taken, so the read checks it
                // rather than paging the wrong history. An anchor that no
                // longer holds yields no page, which the caller reads as one.
                PageRead::Older(anchor) => {
                    if history.anchor_holds(anchor) {
                        history.turns_before(anchor.byte_offset, page_turns, PAGE_MAX_BYTES)
                    } else {
                        TurnPage::default()
                    }
                }
                // The head is the start of the history in hand, not of the
                // file: a cleared session's first turn is the one after the
                // event that began its epoch.
                PageRead::Head => {
                    history.head_turns(epoch, page_turns, PAGE_MAX_BYTES, &worker_cancel)
                }
            };
            // There is no generation tag on the result: one read is tracked at
            // a time, so a page is always the one the pending slot waits for,
            // and whether it still describes the current history is decided by
            // the watermark the read was dispatched with. A superseded read
            // keeps running and finds nobody to take its page.
            if tx.send(page).is_err() {
                // The reader was dropped while the page was being read, so
                // there is nobody left to take it.
            }
        });
        state.pending = Some(PendingPageRead {
            dispatched,
            total_turns,
            read,
            rx,
            cancel,
        });
    }

    /// Drop the read in flight, if any, so the intent that asks for it next is
    /// the one that lands.
    ///
    /// A read serves the intent that dispatched it. When the user asks for
    /// another end of the history, that intent replaces the one in flight: the
    /// old worker is told to stop and its page is left unclaimed, rather than
    /// applied over the window the user just asked for.
    fn supersede(state: &mut TrajectoryState) {
        if let Some(pending) = state.pending.take() {
            pending.cancel.store(true, Ordering::Release);
        }
    }

    /// Whether the read in flight is already the one this intent asks for.
    ///
    /// A held key repeats, and a repeat asks for the same end again: treating
    /// it as a new intent would replace a read that never gets to finish.
    fn pending_is(state: &TrajectoryState, read: PageRead, epoch: Option<EventId>) -> bool {
        state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.read == read && pending.dispatched.epoch == epoch)
    }

    /// Drop the cached view, so the next frame recomputes what the window is.
    ///
    /// Every read that ends without applying its page must call this: the view
    /// was built while the read was in flight, so it reports a load state that
    /// no longer holds, and the cache key (the watermark the view was built
    /// for) still matches, so nothing else would rebuild it.
    fn drop_view(state: &mut TrajectoryState) {
        state.view = None;
        state.view_state = None;
    }

    /// Take a finished page, if one is ready. Called from a draw, so it never
    /// blocks.
    fn drain(&self, state: &mut TrajectoryState, current: DurableWatermark) {
        let Some(pending) = state.pending.as_ref() else {
            return;
        };
        match pending.rx.try_recv() {
            Ok(outcome) => {
                let read = pending.read;
                let dispatched = pending.dispatched;
                let total_turns = pending.total_turns;
                state.pending = None;
                state.failed = false;
                state.read_failures = 0;
                if dispatched.epoch != current.epoch {
                    // The session was cleared while this read was in flight.
                    Self::drop_view(state);
                    return;
                }
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
                            return;
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
                            return;
                        }
                        state.pages.clear();
                        state.pages.push_back(outcome);
                        state.older_hidden = 0;
                        state.follow_tail = false;
                    }
                    PageRead::Tail => {
                        // A window the user walked away from is theirs, so a
                        // tail page read before that does not move it. An empty
                        // window has nothing to protect: it takes the tail and
                        // follows it, which is the only way it leaves loading.
                        if !state.follow_tail && !state.pages.is_empty() {
                            Self::drop_view(state);
                            return;
                        }
                        state.follow_tail = true;
                        state.pages.clear();
                        state.pages.push_back(outcome);
                    }
                }
                state.window_watermark = Some(dispatched);
                state.window_total = total_turns;
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

impl TrajectoryLog for SessionLogTrajectory {
    fn trajectory(&self) -> Arc<TrajectoryView> {
        let head = self.session_log.trajectory_head(self.session_id);
        let Ok(mut state) = self.state.lock() else {
            // No state to read the history from: a failure view carries no
            // rows, so its generation is never compared.
            return self.head_view(&head, TrajectoryViewState::Failed, 0);
        };
        let watermark = watermark_of(&head);
        // A clear starts a new history whose turn numbers begin again, so the
        // count moves with the epoch: a view carries it so a caller can tell
        // whether a turn number still names the same turn.
        if state.seen_epoch != watermark.epoch {
            state.seen_epoch = watermark.epoch;
            state.history_generation = state.history_generation.wrapping_add(1);
        }
        // A failure belongs to the history it happened on: once that history
        // has moved on, or a clear has started a new epoch, the read is worth
        // trying again rather than reporting the old failure forever.
        if state.failed && state.failed_watermark != Some(watermark) {
            state.failed = false;
            state.failed_watermark = None;
            state.read_failures = 0;
        }
        self.drain(&mut state, watermark);
        if state.failed {
            return self.serve(&mut state, &head, TrajectoryViewState::Failed, watermark);
        }
        // A clear starts a new epoch, so whatever is resident describes turns
        // the session no longer counts. Dropping it here is the other half of
        // refusing a stale page in drain: keeping it would show the old epoch
        // for as long as the new read takes.
        if !state.pages.is_empty()
            && state
                .window_watermark
                .is_none_or(|resident| resident.epoch != watermark.epoch)
        {
            state.pages.clear();
            state.window_watermark = None;
            state.follow_tail = true;
            state.older_hidden = 0;
            Self::supersede(&mut state);
            Self::drop_view(&mut state);
        }
        let max_turns = self.max_turns();
        // Only a settled window is served from the cache: while a read is in
        // flight the cached view is the one that says so, and it must be
        // rebuilt (once) rather than returned as the current window.
        // The window is settled when no read is in flight and the cached view
        // was built for this durable history. A live revision is not part of
        // the key: it moves on every streamed token, and re-projecting the
        // window for one would cost a page fold per frame.
        if state.pending.is_none()
            && let Some(view) = state.view.as_ref()
            && state.view_watermark == Some(watermark)
            && state.max_turns == max_turns
        {
            return Arc::clone(view);
        }
        // A backend with no byte-addressable log, such as an in-memory store,
        // has no pages to read. Its mirror holds the whole history already, so
        // it is projected directly rather than paged.
        if !self.history.byte_windows() {
            let events = self.session_log.trajectory_snapshot(self.session_id);
            let mut view = Arc::new(project(&events, &self.model, max_turns));
            let view_mut = Arc::make_mut(&mut view);
            view_mut.state = TrajectoryViewState::Ready;
            view_mut.history_generation = state.history_generation;
            state.window_watermark = Some(watermark);
            state.view_watermark = Some(watermark);
            state.max_turns = max_turns;
            state.view = Some(Arc::clone(&view));
            state.view_state = Some(TrajectoryViewState::Ready);
            return view;
        }
        // A read already in flight for this durable history is left to land:
        // dropping it on every revision change would never finish while the
        // session keeps appending.
        // A read is needed when the window is empty, or when it ends at the
        // tail and the durable history has moved past it. A window the user
        // walked back to is not refreshed: an append there is counted as
        // newer, not read, so it cannot leave a gap or move their rows.
        let needs_read = state.pages.is_empty()
            || (state.window_watermark != Some(watermark) && state.follow_tail);
        if state.pending.is_none() && needs_read {
            self.dispatch(
                &mut state,
                PageRead::Tail,
                watermark,
                head.summary.total_turns,
            );
        }
        state.max_turns = max_turns;
        // Ready once the resident pages were read for this history, even when
        // that page is empty: a session with no turns is settled, not loading.
        let load = if state.pages.is_empty() {
            TrajectoryViewState::Loading
        } else if state.pending.is_some()
            || (state.follow_tail && state.window_watermark != Some(watermark))
        {
            // A read is in flight, or one is due behind a window that follows
            // the tail: the rows stay, and the pane says more is coming.
            TrajectoryViewState::LoadingOlder
        } else {
            TrajectoryViewState::Ready
        };
        self.serve(&mut state, &head, load, watermark)
    }

    fn load_older(&self) {
        // The mirror path widens its own window instead of reading a page.
        if !self.history.byte_windows() {
            self.loaded_turns
                .fetch_add(TRAJECTORY_PAGE_TURNS, Ordering::Relaxed);
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        // The oldest resident page carries the anchor that continues behind
        // the window; the newest page's would re-fetch a page already held.
        let Some(anchor) = state.pages.front().and_then(|page| page.oldest_anchor) else {
            return;
        };
        let head = self.session_log.trajectory_head(self.session_id);
        // A window read before a clear describes turns the session no longer
        // counts. Its anchor belongs to the history the clear ended, and a read
        // stamped with the new history would carry that page past the epoch
        // check, so the window is dropped here instead of walked behind.
        if state
            .window_watermark
            .is_some_and(|resident| resident.epoch != head.revision.epoch_event_id)
        {
            state.pages.clear();
            state.older_hidden = 0;
            state.follow_tail = true;
            state.window_watermark = None;
            Self::supersede(&mut state);
            Self::drop_view(&mut state);
            return;
        }
        // The same walk asked for again is already on its way.
        if Self::pending_is(
            &state,
            PageRead::Older(anchor),
            head.revision.epoch_event_id,
        ) {
            return;
        }
        // Walking back is the moment the window stops following the tail. The
        // count of turns before the window is already the one the last build
        // derived: an append adds turns after the window, so it does not move
        // that count, and freezing it is what keeps an append from recomputing
        // a window the user walked away from.
        state.follow_tail = false;
        Self::supersede(&mut state);
        self.dispatch(
            &mut state,
            PageRead::Older(anchor),
            watermark_of(&head),
            head.summary.total_turns,
        );
    }

    fn load_earliest(&self) {
        // The mirror path has no pages: widen it to the whole history instead.
        if !self.history.byte_windows() {
            self.loaded_turns.store(usize::MAX, Ordering::Relaxed);
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let head = self.session_log.trajectory_head(self.session_id);
        if Self::pending_is(&state, PageRead::Head, head.revision.epoch_event_id) {
            return;
        }
        state.follow_tail = false;
        Self::supersede(&mut state);
        self.dispatch(
            &mut state,
            PageRead::Head,
            watermark_of(&head),
            head.summary.total_turns,
        );
    }

    fn return_to_tail(&self) {
        // The mirror path projects the newest turns, so resetting the width is
        // enough to put the tail back.
        if !self.history.byte_windows() {
            self.loaded_turns
                .store(TRAJECTORY_PAGE_TURNS, Ordering::Relaxed);
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let epoch = self.session_log.trajectory_head(self.session_id);
        if Self::pending_is(&state, PageRead::Tail, epoch.revision.epoch_event_id)
            || (state.follow_tail && !state.pages.is_empty())
        {
            return;
        }
        Self::supersede(&mut state);
        // Dropping the window is what puts the tail back: the next draw sees an
        // empty window, follows the tail again, and reads it.
        state.pages.clear();
        state.older_hidden = 0;
        state.follow_tail = true;
        state.window_watermark = None;
        Self::drop_view(&mut state);
    }
}

/// The durable history a head's revision describes, as the reader keys on it.
fn watermark_of(head: &TrajectoryHead) -> DurableWatermark {
    DurableWatermark {
        epoch: head.revision.epoch_event_id,
        count: head.revision.durable_event_count,
        last_id: head.revision.last_durable_event_id,
    }
}
