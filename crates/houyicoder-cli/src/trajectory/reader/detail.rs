//! One turn's records, read when the user drills into it.
//!
//! The drill holds a key, and this maps it back to the bytes: the anchor is
//! captured when the request is made, so the read finishes even if the window
//! moves the turn out of the resident pages meanwhile.

use std::sync::{Arc, Mutex};

use houyicoder_api::session::SessionLog;
use houyicoder_context::{EventId, SessionId, SessionLogEntry};
use houyicoder_tui::state::{TrajectoryDrill, TrajectoryTurnKey};
use houyicoder_tui::view::trajectory_pane::{
    TrajectoryDetailState, TrajectoryDetailView, TrajectoryRow, TrajectoryTurn,
};

use super::super::turns::FoldMode;
use super::super::view::fold_rows;
use super::TrajectoryState;

use crate::session_history::{SessionHistory, is_user_input};

/// How much of a turn one detail read takes. A turn wider than this comes back
/// with its beginning and says so, rather than showing part of a turn as the
/// whole of it.
const DETAIL_MAX_BYTES: u64 = 1024 * 1024;

/// One turn's records, read when the user drills into it.
///
/// The drill holds a key, and this maps it back to the bytes: the anchor is
/// captured when the request is made, so the read finishes even if the window
/// moves the turn out of the resident pages meanwhile.
#[derive(Default)]
pub(super) struct TrajectoryDetailRead {
    /// The key the cached view answers for.
    pub(super) requested: Option<TrajectoryTurnKey>,
    /// The identity the requested read was made for, kept so a retry has the
    /// number as well as the key.
    pub(super) drill: Option<TrajectoryDrill>,
    pub(super) view: Option<Arc<TrajectoryDetailView>>,
    pub(super) pending: Option<PendingDetailRead>,
    /// The history the resident window was read in, so a read that was in
    /// flight across a clear is dropped rather than answering for a turn this
    /// session no longer has.
    pub(super) epoch: Option<EventId>,
    /// The bytes the requested turn starts at, kept so a read whose worker died
    /// can be asked for again without a new request.
    pub(super) anchor: Option<u64>,
    /// Reads that ended without an answer. One is a transient; a run of them is
    /// a broken read, and the pane says so rather than asking again forever.
    pub(super) failures: usize,
}

/// How many reads may end without an answer before the pane reports failure
/// rather than asking again. One is a transient; a run of them is a broken read.
const READ_FAILURES_BEFORE_FAILED: usize = 3;

/// A detail read in flight.
pub(super) struct PendingDetailRead {
    pub(super) rx: std::sync::mpsc::Receiver<TrajectoryDetailView>,
}

/// One turn's events: from its opening event to the next turn's opening. The
/// flag says whether that next opening was reached, so a caller can tell a whole
/// turn from the beginning of a wider one.
pub(super) fn one_turn<'a>(
    events: impl Iterator<Item = &'a SessionLogEntry>,
) -> (Vec<SessionLogEntry>, bool) {
    let mut out = Vec::new();
    let mut opened = false;
    let mut reached = false;
    for entry in events {
        if opened && is_user_input(entry) {
            reached = true;
            break;
        }
        opened = true;
        out.push(entry.clone());
    }
    (out, reached)
}

/// The byte offset of a turn in the resident window, if it is there.
///
/// The key is the id of the event that opened the turn, and the page's events
/// carry both it and the offset they were read at.
pub(super) fn anchor_of(state: &TrajectoryState, key: &TrajectoryTurnKey) -> Option<u64> {
    state
        .pages
        .iter()
        .flat_map(|page| page.events.iter())
        .find(|event| event.entry.id.to_string() == key.as_str())
        .map(|event| event.byte_offset)
}

/// One turn's records, read from the bytes its key names.
pub(super) fn read_detail(
    history: &SessionHistory,
    offset: u64,
    drill: &TrajectoryDrill,
) -> TrajectoryDetailView {
    let window = history.window(offset, DETAIL_MAX_BYTES);
    let Some(first) = window.events.first() else {
        // No line at the anchor: the opening event is wider than one read can
        // hold, so the turn cannot be read at all. That is a failed read, not a
        // key that names nothing.
        return failed_detail();
    };
    // The bytes at the anchor must still be the event the key names: a log
    // rewritten under it would otherwise answer with another turn.
    if first.entry.id.to_string() != drill.key.as_str() {
        return stale_detail();
    }
    let (events, reached_end) = one_turn(window.events.iter().map(|event| &event.entry));
    let truncated = !reached_end && window.next_offset < history.log_size();
    let (rows, records) = fold_rows(&events, drill.number, FoldMode::Records);
    let mut turn = first_turn_of(rows);
    if let Some(turn) = turn.as_mut() {
        // The bytes do not know which turn of the session they are, so the
        // number comes from the identity the drill carries.
        turn.n = drill.number;
    }
    TrajectoryDetailView {
        state: TrajectoryDetailState::Ready,
        turn,
        records: records
            .into_iter()
            .next()
            .map(|(_, r)| r)
            .unwrap_or_default(),
        truncated,
    }
}

/// The turn a fold produced, when it produced one.
fn first_turn_of(rows: Vec<TrajectoryRow>) -> Option<TrajectoryTurn> {
    rows.into_iter().find_map(|row| match row {
        TrajectoryRow::Turn(turn) => Some(turn),
        TrajectoryRow::Bg(_) => None,
    })
}

/// The mirror path holds the whole history, so it answers without a read.
pub(super) fn mirror_detail(
    session_log: &dyn SessionLog,
    session_id: SessionId,
    drill: &TrajectoryDrill,
) -> TrajectoryDetailView {
    let events = session_log.trajectory_snapshot(session_id);
    let Some(start) = events
        .iter()
        .position(|entry| entry.id.to_string() == drill.key.as_str())
    else {
        return stale_detail();
    };
    let (events, _) = one_turn(events[start..].iter());
    let (rows, records) = fold_rows(&events, drill.number, FoldMode::Records);
    let mut turn = first_turn_of(rows);
    if let Some(turn) = turn.as_mut() {
        turn.n = drill.number;
    }
    TrajectoryDetailView {
        state: TrajectoryDetailState::Ready,
        turn,
        records: records
            .into_iter()
            .next()
            .map(|(_, r)| r)
            .unwrap_or_default(),
        truncated: false,
    }
}

/// A detail that says the read did not produce the turn's records.
pub(super) fn failed_detail() -> TrajectoryDetailView {
    TrajectoryDetailView {
        state: TrajectoryDetailState::Failed,
        ..TrajectoryDetailView::default()
    }
}

/// A detail that says the key names a turn this history cannot resolve.
pub(super) fn stale_detail() -> TrajectoryDetailView {
    TrajectoryDetailView {
        state: TrajectoryDetailState::Stale,
        ..TrajectoryDetailView::default()
    }
}

/// Ask for one turn's records, capturing the bytes it is read from.
///
/// The anchor is captured here, so the read finishes even if the window moves
/// the turn out of the resident pages while it is in flight.
pub(super) fn request(
    detail: &Mutex<TrajectoryDetailRead>,
    pages: &Mutex<TrajectoryState>,
    history: &Arc<SessionHistory>,
    session_log: &Arc<dyn SessionLog>,
    session_id: SessionId,
    drill: &TrajectoryDrill,
) {
    let Ok(mut state) = detail.lock() else {
        return;
    };
    if state.requested.as_ref() == Some(&drill.key) {
        // A draw asks again every frame; the first ask is the request. A read
        // in flight, a view in hand, and a failure the pane is showing all mean
        // there is nothing to ask for.
        return;
    }
    let epoch = session_log
        .trajectory_head(session_id)
        .revision
        .epoch_event_id;
    // The mirror path holds the whole history, so it answers at once.
    if !history.byte_windows() {
        let view = mirror_detail(session_log.as_ref(), session_id, drill);
        state.requested = Some(drill.key.clone());
        state.drill = Some(drill.clone());
        state.epoch = epoch;
        state.anchor = None;
        state.failures = 0;
        state.view = Some(Arc::new(view));
        state.pending = None;
        return;
    }
    let anchor = pages
        .lock()
        .ok()
        .and_then(|pages| anchor_of(&pages, &drill.key));
    state.requested = Some(drill.key.clone());
    state.drill = Some(drill.clone());
    state.epoch = epoch;
    state.anchor = anchor;
    state.failures = 0;
    // A view for the turn the user just left is not the turn they are on: it is
    // dropped here, so a draw while the new read is in flight cannot show it.
    state.view = None;
    state.pending = None;
    let Some(offset) = anchor else {
        // The window moved past the turn, so there are no bytes to read:
        // the key names a turn this window cannot resolve.
        state.view = Some(Arc::new(stale_detail()));
        return;
    };
    state.pending = Some(dispatch_read(history, drill.clone(), offset));
}

/// Start one detail read on a worker, and hand back what to poll for it.
fn dispatch_read(
    history: &Arc<SessionHistory>,
    drill: TrajectoryDrill,
    offset: u64,
) -> PendingDetailRead {
    let history = Arc::clone(history);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The reader may be gone; the read is bounded, so the worker finishes
        // and its result is dropped.
        drop(tx.send(read_detail(&history, offset, &drill)));
    });
    PendingDetailRead { rx }
}

/// What is known about a turn's records: what the read has produced so far.
pub(super) fn detail(
    detail: &Mutex<TrajectoryDetailRead>,
    pages: &Mutex<TrajectoryState>,
    history: &Arc<SessionHistory>,
    key: &TrajectoryTurnKey,
) -> Arc<TrajectoryDetailView> {
    let Ok(mut state) = detail.lock() else {
        return Arc::new(TrajectoryDetailView::default());
    };
    if state.requested.as_ref() != Some(key) {
        // A key that was never asked for: the honest answer is that it is
        // not this window's to show.
        return Arc::new(stale_detail());
    }
    let resident_epoch = pages
        .lock()
        .ok()
        .and_then(|pages| pages.window_watermark)
        .and_then(|watermark| watermark.epoch);
    // A window being read again has no watermark yet: that is a read in flight,
    // not a history that changed. Only a watermark naming another history makes
    // the detail stale, so a retry does not report the drill as gone.
    if let Some(resident) = resident_epoch
        && state.epoch != Some(resident)
    {
        state.pending = None;
        state.view = Some(Arc::new(stale_detail()));
        return Arc::new(stale_detail());
    }
    let arrived = state.pending.as_ref().map(|pending| pending.rx.try_recv());
    match arrived {
        Some(Ok(view)) => {
            state.pending = None;
            state.view = Some(Arc::new(view));
        }
        Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
            // The worker died without sending. One dead worker is a transient,
            // so the read is asked for again from the anchor already captured;
            // a run of them means the reads are not working, and the pane says
            // so rather than serving a loading state nothing will fill.
            state.pending = None;
            state.failures += 1;
            if state.failures < READ_FAILURES_BEFORE_FAILED
                && let (Some(offset), Some(drill)) = (state.anchor, state.drill.clone())
            {
                state.pending = Some(dispatch_read(history, drill, offset));
                return state
                    .view
                    .clone()
                    .unwrap_or_else(|| Arc::new(TrajectoryDetailView::default()));
            }
            state.view = Some(Arc::new(failed_detail()));
        }
        _ => {}
    }
    state
        .view
        .clone()
        .unwrap_or_else(|| Arc::new(TrajectoryDetailView::default()))
}
