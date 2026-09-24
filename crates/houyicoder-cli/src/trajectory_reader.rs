//! The trajectory pane's page reader.
//!
//! The turn list comes from a bounded page of the durable log, read by a
//! worker thread and projected into the view the pane draws. The reader itself
//! never touches the disk on a draw: it serves the window it holds and reports
//! a read in flight as loading. Projection lives in the view module; this one
//! owns the read state and when a read is needed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use houyicoder_api::session::{SessionLog, TrajectoryHead};
use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use houyicoder_tui::view::trajectory_pane::{
    SessionTiming, SubagentUsage, TrajectoryLog, TrajectoryRow, TrajectoryView, TrajectoryViewState,
};

use crate::session_history::{PAGE_MAX_BYTES, SessionHistory, TurnPage};
use crate::trajectory_view::{project, project_rows};

/// How many turns the pane loads by default, and how many it adds each time
/// the user asks for older history.
pub(crate) const TRAJECTORY_PAGE_TURNS: usize = 100;

/// How many pages stay resident. A provisional bound: the window slides once
/// the selection is anchored to a durable turn, which is what lets a page be
/// dropped without moving the row the user is on.
const RESIDENT_PAGES: usize = 2;

/// How many reads may end without a page before the pane reports failure
/// rather than retrying. One is a transient; a run of them is a broken read,
/// and retrying it every frame would spawn a worker per frame.
const READ_FAILURES_BEFORE_FAILED: usize = 3;

/// A page read handed back by the worker thread.
///
/// There is no generation tag: at most one read is in flight, so a result is
/// always the one the pending slot is waiting for, and whether it still
/// describes the current history is decided by the watermark it carries.
struct PageOutcome {
    page: Box<TurnPage>,
}

/// The pane's read state: the page it holds, the view projected from it, and
/// the job in flight, if any.
///
/// The pane draws every frame, so nothing here reads the log. A page arrives
/// from a worker and is applied on a later frame; until it does, the view
/// reports that it is loading rather than showing an empty list.
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
    epoch: Option<houyicoder_context::EventId>,
    count: usize,
    last_id: Option<houyicoder_context::EventId>,
}

/// A page read in flight, and what it was dispatched for.
struct PendingPageRead {
    /// The durable history this read was dispatched for.
    dispatched: DurableWatermark,
    older: Option<u64>,
    rx: std::sync::mpsc::Receiver<PageOutcome>,
}

#[derive(Default)]
struct TrajectoryState {
    /// Newest first: the tail page, then any older page loaded behind it.
    pages: Vec<TurnPage>,
    view: Option<Arc<TrajectoryView>>,
    /// The state the cached view was built for, so a frame while a read is in
    /// flight serves it instead of rebuilding the window.
    view_state: Option<TrajectoryViewState>,
    /// The durable history the resident pages were read for. A streaming delta
    /// moves the mirror revision but not this, so keying on it is what keeps a
    /// stream from re-reading the log once per frame.
    ///
    /// The consequence, deliberate: the header's figures refresh at durable
    /// event boundaries rather than per streamed token, so the session's
    /// elapsed seconds hold steady while a model is mid-answer. The
    /// alternative is rebuilding the view on every token, which is a page fold
    /// and a set of allocations on the draw path.
    read_watermark: Option<DurableWatermark>,
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
    /// Convenience for tests, which build a reader without a shared history.
    #[cfg(test)]
    pub fn new(session_log: Arc<dyn SessionLog>, session_id: SessionId, model: String) -> Self {
        Self::with_history(
            Arc::new(SessionHistory::new(session_log.clone(), session_id)),
            session_log,
            session_id,
            model,
        )
    }

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
            state: Mutex::new(TrajectoryState::default()),
            loaded_turns: AtomicUsize::new(TRAJECTORY_PAGE_TURNS),
        }
    }

    /// The history reader this view pages, for a test to assert on what it
    /// asked the disk for.
    #[cfg(test)]
    pub(crate) fn history(&self) -> &SessionHistory {
        &self.history
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
        older: Option<u64>,
        dispatched: DurableWatermark,
    ) {
        let history = self.history.clone();
        let page_turns = self.max_turns();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let page = match older {
                Some(anchor) => history.turns_before(anchor, page_turns, PAGE_MAX_BYTES),
                None => history.tail_turns(page_turns, PAGE_MAX_BYTES),
            };
            if tx
                .send(PageOutcome {
                    page: Box::new(page),
                })
                .is_err()
            {
                // The reader was dropped while the page was being read, so
                // there is nobody left to take it.
            }
        });
        state.pending = Some(PendingPageRead {
            dispatched,
            older,
            rx,
        });
    }

    /// Take a finished page, if one is ready. Called from a draw, so it never
    /// blocks.
    fn drain(&self, state: &mut TrajectoryState, current: DurableWatermark) {
        let Some(pending) = state.pending.as_ref() else {
            return;
        };
        match pending.rx.try_recv() {
            Ok(outcome) => {
                let older = pending.older;
                let dispatched = pending.dispatched;
                state.pending = None;
                state.failed = false;
                state.read_failures = 0;
                if dispatched.epoch != current.epoch {
                    // The session was cleared while this read was in flight.
                    // Its page describes the epoch before the clear, so it is
                    // dropped rather than shown and corrected a frame later.
                    return;
                }
                match older {
                    // An older page is byte-anchored, so a later append does not
                    // invalidate it: it still abuts the tail it was read behind.
                    Some(_) => {
                        if state.pages.len() < RESIDENT_PAGES {
                            state.pages.push(*outcome.page);
                        }
                    }
                    None => {
                        // A page behind the current history is still applied:
                        // a window slightly older than the log beats a pane
                        // that never leaves loading while a session appends
                        // faster than a page can be read. The next dispatch
                        // catches up.
                        //
                        // Whatever was loaded behind the old tail is dropped,
                        // because the new tail starts later and the two would
                        // leave a gap of turns that were never read.
                        state.pages.clear();
                        state.pages.push(*outcome.page);
                        state.read_watermark = Some(dispatched);
                    }
                }
                state.view = None;
                state.view_state = None;
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
                state.read_watermark = None;
                if state.read_failures >= READ_FAILURES_BEFORE_FAILED {
                    state.failed = true;
                    state.failed_watermark = Some(dispatched);
                }
            }
        }
    }

    /// The window's events in log order: older pages first, the tail last.
    ///
    /// A page whose oldest turn was cut short by the byte budget carries a
    /// fragment of a turn, so everything before the window's first user input
    /// is dropped: rendering it would invent a turn the session never had.
    fn window_events(state: &TrajectoryState) -> Vec<SessionLogEntry> {
        let mut events: Vec<SessionLogEntry> = state
            .pages
            .iter()
            .rev()
            .flat_map(|page| page.events.iter())
            .map(|located| located.entry.clone())
            .collect();
        if state.pages.last().is_some_and(|page| page.oldest_partial)
            && let Some(cut) = events
                .iter()
                .position(|entry| matches!(entry.event, SessionEvent::UserInput { .. }))
        {
            events.drain(..cut);
        }
        events
    }

    /// Build the view for the window in hand: rows from the page, every
    /// session figure from the head's summary.
    ///
    /// The header answers what the session spent, so it cannot be computed
    /// from the page: a page holds the newest turns, and its own totals would
    /// report the page as the session.
    fn build_view(
        &self,
        state: &mut TrajectoryState,
        head: &TrajectoryHead,
        load: TrajectoryViewState,
    ) -> Arc<TrajectoryView> {
        if state.pages.is_empty() {
            // Cached like any other state: a settled empty session would
            // otherwise build a fresh view on every frame.
            let view = self.head_view(head, load);
            state.view = Some(Arc::clone(&view));
            return view;
        }
        let events = Self::window_events(state);
        // Number from the rows the projection actually produced, not from the
        // user inputs in the events: a window that opens mid-run yields a turn
        // the fold numbers but no user input counts, and subtracting that turn
        // as if it were hidden would overstate what is behind the window.
        let mut rows = project_rows(&events, 1);
        let visible = rows
            .iter()
            .filter(|row| matches!(row, TrajectoryRow::Turn(_)))
            .count();
        let hidden = head.summary.total_turns.saturating_sub(visible);
        let mut number = hidden;
        for row in rows.iter_mut() {
            if let TrajectoryRow::Turn(turn) = row {
                number += 1;
                turn.n = number;
            }
        }
        let mut view = self.head_view(head, load);
        let value = Arc::make_mut(&mut view);
        value.hidden_turns = hidden;
        value.skipped_records = state.pages.iter().map(|page| page.skipped).sum();
        value.rows = rows;
        state.view = Some(Arc::clone(&view));
        view
    }

    /// The view for a state, built once and then served from the cache.
    ///
    /// A draw must not rebuild the window: while a read is in flight the state
    /// is unchanged frame to frame, and re-projecting the page each time would
    /// put a page fold on the draw path.
    fn serve(
        &self,
        state: &mut TrajectoryState,
        head: &TrajectoryHead,
        load: TrajectoryViewState,
    ) -> Arc<TrajectoryView> {
        if let Some(view) = state.view.as_ref()
            && state.view_state == Some(load)
        {
            return Arc::clone(view);
        }
        let view = self.build_view(state, head, load);
        state.view_state = Some(load);
        view
    }

    /// A view that carries the session's figures and no rows, for the states
    /// where there is nothing truthful to list yet.
    fn head_view(&self, head: &TrajectoryHead, load: TrajectoryViewState) -> Arc<TrajectoryView> {
        let summary = &head.summary;
        Arc::new(TrajectoryView {
            session_id: self.session_id.to_string(),
            // The session's own model count, not the construction-time string:
            // a session that switched models must say so.
            model: match summary.models_used {
                0 => self.model.clone(),
                1 => summary
                    .single_model
                    .as_ref()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| self.model.clone()),
                n => format!("{n} models"),
            },
            total_turns: summary.total_turns,
            models_used: summary.models_used,
            tokens_in: summary
                .usage
                .totals_known
                .then_some(summary.usage.input_tokens as usize),
            tokens_out: summary
                .usage
                .totals_known
                .then_some(summary.usage.output_tokens as usize),
            cache_read: (summary.usage.cache_read_tokens > 0)
                .then_some(summary.usage.cache_read_tokens),
            failures: summary.usage.failures,
            tool_calls: summary.usage.tool_calls,
            duration_secs: summary.duration_ms / 1000,
            timing: SessionTiming {
                ttft_samples: summary.timing.ttft_samples,
                ttft_avg_ms: summary.timing.ttft_avg_ms,
                ttft_p95_ms: summary.timing.ttft_p95_ms,
                ttft_p99_ms: summary.timing.ttft_p99_ms,
                decode_samples: summary.timing.decode_samples,
                decode_tok_per_sec: summary.timing.decode_tok_per_sec,
                model_ms: summary.timing.model_ms,
                tool_ms: summary.timing.tool_ms,
            },
            hidden_turns: summary.total_turns,
            subagent_usage: (summary.usage.subagent.calls > 0).then_some(SubagentUsage {
                calls: summary.usage.subagent.calls,
                input: summary.usage.subagent.input_tokens,
                output: summary.usage.subagent.output_tokens,
                cache_read: summary.usage.subagent.cache_read_input_tokens,
            }),
            state: load,
            skipped_records: 0,
            rows: Vec::new(),
        })
    }
}

impl TrajectoryLog for SessionLogTrajectory {
    fn trajectory(&self) -> Arc<TrajectoryView> {
        let head = self.session_log.trajectory_head(self.session_id);
        let Ok(mut state) = self.state.lock() else {
            return self.head_view(&head, TrajectoryViewState::Failed);
        };
        let watermark = DurableWatermark {
            epoch: head.revision.epoch_event_id,
            count: head.revision.durable_event_count,
            last_id: head.revision.last_durable_event_id,
        };
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
            return self.serve(&mut state, &head, TrajectoryViewState::Failed);
        }
        // A clear starts a new epoch, so whatever is resident describes turns
        // the session no longer counts. Dropping it here is the other half of
        // refusing a stale page in drain: keeping it would show the old epoch
        // for as long as the new read takes.
        if !state.pages.is_empty()
            && state
                .read_watermark
                .is_none_or(|resident| resident.epoch != watermark.epoch)
        {
            state.pages.clear();
            state.read_watermark = None;
            state.view = None;
            state.view_state = None;
        }
        let max_turns = self.max_turns();
        // Only a settled window is served from the cache: while a read is in
        // flight the cached view is the one that says so, and it must be
        // rebuilt (once) rather than returned as the current window.
        // The window is settled when no read is in flight and the resident
        // pages were read for this durable history. A live revision is not
        // part of the key: it moves on every streamed token, and re-projecting
        // the window for one would cost a page fold per frame.
        if state.pending.is_none()
            && let Some(view) = state.view.as_ref()
            && state.read_watermark == Some(watermark)
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
            state.read_watermark = Some(watermark);
            state.max_turns = max_turns;
            state.view = Some(Arc::clone(&view));
            state.view_state = Some(TrajectoryViewState::Ready);
            return view;
        }
        // A read already in flight for this durable history is left to land:
        // dropping it on every revision change would never finish while the
        // session keeps appending.
        // A read is needed when the window is empty or the durable history has
        // moved past what it was read for. Dispatching whenever nothing is in
        // flight would re-read the same history forever.
        let needs_read = state.pages.is_empty() || state.read_watermark != Some(watermark);
        if state.pending.is_none() && needs_read {
            self.dispatch(&mut state, None, watermark);
        }
        state.max_turns = max_turns;
        // Ready once the resident pages were read for this history, even when
        // that page is empty: a session with no turns is settled, not loading.
        let load = if state.read_watermark != Some(watermark) {
            if state.pages.is_empty() {
                TrajectoryViewState::Loading
            } else {
                // Rows from the previous read stay on screen while the next
                // one lands, so the window does not blink empty.
                TrajectoryViewState::LoadingOlder
            }
        } else if state.pending.is_some() {
            // The window is current and an older page is on its way: the rows
            // stay, and the pane says more is coming.
            TrajectoryViewState::LoadingOlder
        } else {
            TrajectoryViewState::Ready
        };
        self.serve(&mut state, &head, load)
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
        if state.pending.is_some() || state.pages.len() >= RESIDENT_PAGES {
            return;
        }
        // The oldest resident page carries the anchor that continues behind
        // the window; the tail's would re-fetch a page already held.
        let Some(anchor) = state.pages.last().and_then(|page| page.older_anchor) else {
            return;
        };
        let head = self.session_log.trajectory_head(self.session_id);
        self.dispatch(
            &mut state,
            Some(anchor),
            DurableWatermark {
                epoch: head.revision.epoch_event_id,
                count: head.revision.durable_event_count,
                last_id: head.revision.last_durable_event_id,
            },
        );
    }
}
