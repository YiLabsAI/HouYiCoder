//! Session-scoped ordering for frontend events.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use houyicoder_api::agent_event::{
    self, AgentEventHandlers, EventHandler, MemoryChangedEvent, ResponseStreamEvent,
    ToolExecutionEvent, UserNoticeEvent,
};
use houyicoder_api::session::TrajectoryPosition;
use houyicoder_context::MemoryScope;
use houyicoder_core::agent::Runner;
use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
use houyicoder_protocol::envelope::{DurableEventId, EventEnvelope, EventSeq};
use houyicoder_protocol::frontend::FrontendEvent;
use houyicoder_protocol::frontend::memory::{
    MemoryChange, MemoryChangeCausality, MemoryChangeId, MemoryChangeOrigin, MemoryChangeScope,
    MemoryOperation,
};
use tokio::sync::Notify;

const MAX_EPHEMERAL_PENDING: usize = 256;

/// The bytes of reliable frames a session retains for a reconnect. A frame the
/// durable log produced is read back from the log on demand, while a notice a
/// producer pushed at runtime exists only while this window holds it, so the
/// budget is sized to keep a session's runtime notice content reachable.
const RELIABLE_WINDOW_BYTES: usize = 8 * 1024 * 1024;

/// Fixed cost of one retained frame: the envelope, its identity, and the
/// queue slot it occupies.
const FRAME_SLOT_BYTES: usize = 64;

#[derive(Clone)]
struct PendingEvent {
    order: u64,
    payload: FrontendEvent,
    reliable: bool,
}

/// The payloads one durable log entry maps to, plus the entry identity
/// stamped on every envelope built from them. Push order stays on the seq
/// axis; the identity is what survives a reconnect, so the two travel
/// together from the load callback but answer different questions.
pub(crate) struct EntryPayloads {
    pub(crate) event_id: DurableEventId,
    pub(crate) payloads: Vec<FrontendEvent>,
}

/// One bounded read of a session's durable log: the entries read from a
/// position, where the read stopped, and whether the walk over the log is
/// finished, either because the history ran out or because a read failed and
/// ended it. The reader reports this apart from the entries because a history
/// longer than one read has to be walked in further reads, while a finished
/// walk has nothing behind it.
pub(crate) struct DurableBatch {
    pub(crate) entries: Vec<EntryPayloads>,
    /// Where the next read resumes, as the walk left it: the ordinal of the
    /// record after the last entry read, and the byte position the log held
    /// when that record was reached. Reading a session's history in rounds
    /// costs a walk from the epoch start per round unless each round is told
    /// the byte position the round before it read to.
    pub(crate) next: TrajectoryPosition,
    pub(crate) walk_finished: bool,
}

/// A replay that has begun and not finished: the frames the window answers the
/// client's cursor with, and the payloads only memory holds, kept until the
/// log's last batch has been sequenced.
pub(crate) struct ReplayPlan {
    /// The retained frames the client is missing, sent ahead of the log's
    /// records. None when the window cannot answer the cursor, where the log
    /// itself carries the whole history back to the client.
    opening: Option<Vec<EventEnvelope>>,
    /// The payloads the durable log cannot reproduce, sequenced after its last
    /// batch so the client's cursor still advances over them.
    held: Vec<FrontendEvent>,
}

/// The newest reliable frames a session keeps in memory, bounded by the bytes
/// they carry.
///
/// A reconnect the window still covers is answered from here. Where it does not
/// cover the client's cursor, the durable log answers instead, and the two
/// sources are not interchangeable: a frame derived from the log can be read
/// out of it again, while a notice a producer pushed at runtime can only come
/// from this window, which is what bounds how far back that content survives.
struct ReliableWindow {
    frames: VecDeque<EventEnvelope>,
    bytes: usize,
    budget: usize,
    floor: Option<EventSeq>,
}

impl Default for ReliableWindow {
    /// A window holding the bytes a session's runtime notices are expected to
    /// occupy.
    fn default() -> Self {
        Self::new(RELIABLE_WINDOW_BYTES)
    }
}

impl ReliableWindow {
    /// Create an empty window holding at most this many bytes of frames.
    fn new(budget: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            bytes: 0,
            budget,
            floor: None,
        }
    }

    /// Append one frame, dropping the oldest frames while the window is over
    /// its budget. The newest frame always stays, so a frame larger than the
    /// whole budget is retained rather than dropped on arrival.
    fn retain(&mut self, frame: EventEnvelope) {
        self.bytes += FRAME_SLOT_BYTES + frame.payload.estimated_bytes();
        self.frames.push_back(frame);
        while self.frames.len() > 1 && self.bytes > self.budget {
            let dropped = self
                .frames
                .pop_front()
                .expect("a window of more than one frame yields its oldest");
            self.bytes -= FRAME_SLOT_BYTES + dropped.payload.estimated_bytes();
            self.floor = Some(dropped.seq);
        }
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
        self.floor = None;
    }

    /// Whether the window still holds every frame a client that last saw this
    /// cursor is missing. A client with no cursor has seen nothing, so only the
    /// log answers it, and once frames have been dropped only a cursor at or
    /// after the oldest dropped frame is answered from here.
    fn covers(&self, last: Option<EventSeq>) -> bool {
        last.is_some_and(|seq| self.floor.is_none_or(|floor| seq >= floor))
    }

    /// The retained frames a client that last saw this cursor is missing, in
    /// the order they were sequenced.
    fn after(&self, last: Option<EventSeq>) -> Vec<EventEnvelope> {
        self.frames
            .iter()
            .filter(|frame| after_cursor(frame.seq, last))
            .cloned()
            .collect()
    }

    /// The retained payloads the durable log cannot reproduce. A frame a log
    /// entry produced carries that entry's identity, so reading the log again
    /// delivers it; a frame a producer pushed at runtime carries none, and this
    /// is the only place it is held.
    fn runtime_payloads(&self) -> Vec<FrontendEvent> {
        self.frames
            .iter()
            .filter(|frame| frame.event_id.is_none())
            .map(|frame| frame.payload.clone())
            .collect()
    }
}

#[derive(Default)]
struct SequencerState {
    next_seq: u64,
    next_pending_order: u64,
    /// The position the next durable read starts at. Its ordinal is the
    /// absolute index of the record that read begins at, counted over the
    /// whole log rather than over any window, and the byte position rides
    /// beside it so the log is read where it was left rather than searched
    /// from its start.
    trajectory_position: TrajectoryPosition,
    pending: VecDeque<PendingEvent>,
    pending_status: HashMap<String, PendingEvent>,
    ephemeral_pending: usize,
    window: ReliableWindow,
    latest_status: HashMap<String, EventEnvelope>,
}

struct SequencerInner {
    state: Mutex<SequencerState>,
    notify: Notify,
}

fn agent_status_id(event: &FrontendEvent) -> Option<&str> {
    match event {
        FrontendEvent::AgentStatus { agent_id, .. } => Some(agent_id),
        _ => None,
    }
}

/// Whether a frame is newer than the last one a client acknowledged.
fn after_cursor(seq: EventSeq, last: Option<EventSeq>) -> bool {
    last.is_none_or(|seen| seq > seen)
}

/// The per-session ordering authority for frontend events.
///
/// Producers enqueue typed events without assigning sequence numbers. The
/// active Server is the sole consumer and frontend event writer, so sequence
/// allocation and carrier order cannot diverge. Reliable frames are retained
/// for a reconnect to the same server process in a window bounded by the bytes
/// those frames carry; a cursor the window no longer covers is answered from
/// the durable log, with the frames only memory holds delivered alongside it.
/// Ephemeral previews may be dropped while their queue is full.
#[derive(Clone)]
pub struct EventSequencer {
    inner: Arc<SequencerInner>,
}

impl EventHandler<ResponseStreamEvent> for EventSequencer {
    fn handle(&self, event: ResponseStreamEvent) {
        let (method, text) = match event {
            ResponseStreamEvent::AssistantTextDelta { text } => (AcpxMethod::LlmTextDelta, text),
            ResponseStreamEvent::ReasoningDelta { text } => (AcpxMethod::LlmReasoningDelta, text),
        };
        self.enqueue_ephemeral(FrontendEvent::Acpx {
            notification: AcpxNotification::new(method, serde_json::json!({ "text": text })),
        });
    }
}

impl EventHandler<ToolExecutionEvent> for EventSequencer {
    fn handle(&self, event: ToolExecutionEvent) {
        let ToolExecutionEvent::Progress {
            call_id,
            elapsed_secs,
            output_lines,
        } = event;
        self.enqueue_ephemeral(FrontendEvent::Acpx {
            notification: AcpxNotification::new(
                AcpxMethod::ToolProgress,
                serde_json::json!({ "call_id": call_id, "elapsed_secs": elapsed_secs, "lines": output_lines }),
            ),
        });
    }
}

impl EventHandler<MemoryChangedEvent> for EventSequencer {
    fn handle(&self, event: MemoryChangedEvent) {
        // The notice names the turn, not the producer, so the producer is
        // recorded here rather than shown.
        tracing::debug!(
            origin = ?event.origin,
            causality = ?event.causality,
            changes = event.changes.len(),
            "memory change notice"
        );
        self.enqueue_reliable(FrontendEvent::MemoryChanged {
            id: MemoryChangeId(event.id.to_string()),
            origin: to_protocol_memory_origin(event.origin),
            causality: to_protocol_memory_causality(event.causality),
            changes: event
                .changes
                .into_iter()
                .map(|change| MemoryChange {
                    key: change.key,
                    operation: to_protocol_memory_operation(change.operation),
                    scope: to_protocol_memory_scope(change.scope),
                })
                .collect(),
        });
    }
}

impl EventHandler<UserNoticeEvent> for EventSequencer {
    fn handle(&self, event: UserNoticeEvent) {
        self.enqueue_reliable(FrontendEvent::SystemLine {
            text: event.message,
        });
    }
}

fn to_protocol_memory_origin(origin: agent_event::MemoryChangeOrigin) -> MemoryChangeOrigin {
    match origin {
        agent_event::MemoryChangeOrigin::PrimaryAgent => MemoryChangeOrigin::PrimaryAgent,
        agent_event::MemoryChangeOrigin::AutoMemory => MemoryChangeOrigin::AutoMemory,
        agent_event::MemoryChangeOrigin::AutoDream => MemoryChangeOrigin::AutoDream,
    }
}

fn to_protocol_memory_causality(
    causality: agent_event::MemoryChangeCausality,
) -> MemoryChangeCausality {
    match causality {
        agent_event::MemoryChangeCausality::ThisTurn => MemoryChangeCausality::ThisTurn,
        agent_event::MemoryChangeCausality::PreviousTurn => MemoryChangeCausality::PreviousTurn,
    }
}

fn to_protocol_memory_operation(operation: agent_event::MemoryOperation) -> MemoryOperation {
    match operation {
        agent_event::MemoryOperation::Created => MemoryOperation::Created,
        agent_event::MemoryOperation::Updated => MemoryOperation::Updated,
        agent_event::MemoryOperation::Deleted => MemoryOperation::Deleted,
        agent_event::MemoryOperation::Promoted => MemoryOperation::Promoted,
        agent_event::MemoryOperation::Demoted => MemoryOperation::Demoted,
    }
}

fn to_protocol_memory_scope(scope: MemoryScope) -> MemoryChangeScope {
    match scope {
        MemoryScope::User => MemoryChangeScope::User,
        MemoryScope::Project => MemoryChangeScope::Project,
        MemoryScope::Auto => MemoryChangeScope::Auto,
    }
}

impl Default for EventSequencer {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSequencer {
    /// Create an empty per-session event stream.
    pub fn new() -> Self {
        Self::with_window(RELIABLE_WINDOW_BYTES)
    }

    /// Create an empty stream whose retained window holds at most this many
    /// bytes of reliable frames.
    fn with_window(reliable_window_bytes: usize) -> Self {
        Self {
            inner: Arc::new(SequencerInner {
                state: Mutex::new(SequencerState {
                    window: ReliableWindow::new(reliable_window_bytes),
                    ..SequencerState::default()
                }),
                notify: Notify::new(),
            }),
        }
    }

    /// Install this sequencer as the runner's event destination.
    pub fn install_on(&self, runner: &mut Runner) {
        let mut handlers = AgentEventHandlers::default();
        handlers.set_response_stream(Arc::new(self.clone()));
        handlers.set_tool_execution(Arc::new(self.clone()));
        handlers.set_memory_changed(Arc::new(self.clone()));
        handlers.set_user_notice(Arc::new(self.clone()));
        runner.set_event_handlers(handlers);
    }

    /// Enqueue a reliable event for the active server to sequence and retain.
    pub fn enqueue_reliable(&self, payload: FrontendEvent) {
        self.enqueue(payload, true);
    }

    /// Enqueue an ephemeral preview without blocking its producer.
    pub fn enqueue_ephemeral(&self, payload: FrontendEvent) {
        self.enqueue(payload, false);
    }

    fn enqueue(&self, payload: FrontendEvent, reliable: bool) {
        let mut state = self.inner.state.lock().expect("event sequencer lock");
        if !reliable && state.ephemeral_pending >= MAX_EPHEMERAL_PENDING {
            return;
        }
        let order = state.next_pending_order;
        state.next_pending_order = state.next_pending_order.saturating_add(1);
        let event = PendingEvent {
            order,
            payload,
            reliable,
        };
        if reliable && let Some(agent_id) = agent_status_id(&event.payload).map(str::to_owned) {
            state.pending_status.insert(agent_id, event);
            drop(state);
            self.inner.notify.notify_one();
            return;
        }
        if !reliable {
            state.ephemeral_pending += 1;
        }
        state.pending.push_back(event);
        drop(state);
        self.inner.notify.notify_one();
    }

    pub(crate) fn notified(&self) -> impl Future<Output = ()> + '_ {
        self.inner.notify.notified()
    }

    pub(crate) fn reset_sequencer(&self) {
        let mut state = self.inner.state.lock().expect("event sequencer lock");
        state.trajectory_position = TrajectoryPosition::start();
        state.next_pending_order = 0;
        state.pending.clear();
        state.pending_status.clear();
        state.ephemeral_pending = 0;
        state.window.clear();
        state.latest_status.clear();
    }

    /// Sequence the durable events one bounded batch of the log maps, and the
    /// pending runtime events once that batch was the history's last. Reports
    /// whether the log has been read to its end.
    ///
    /// The lock is held for one batch rather than for a walk of the whole
    /// history, so a producer appending while a long session is read waits for
    /// a batch instead of for the walk.
    pub(crate) fn sequence_available<F>(&self, load_durable_events: F) -> (Vec<EventEnvelope>, bool)
    where
        F: FnOnce(TrajectoryPosition) -> DurableBatch,
    {
        let mut state = self.inner.state.lock().expect("event sequencer lock");
        let batch = load_durable_events(state.trajectory_position);
        let DurableBatch {
            entries,
            next,
            walk_finished,
        } = batch;
        let mut sequenced = Self::sequence_trajectory(&mut state, entries);
        state.trajectory_position = next;
        if walk_finished {
            sequenced.extend(Self::sequence_pending(&mut state));
        }
        (sequenced, walk_finished)
    }

    /// Begin a replay for a client that last saw the given sequence.
    ///
    /// A cursor the window still covers is answered from the window, and the
    /// frames the client is missing are lifted out here, before any load can
    /// trim the window's front out from under them. A cursor it cannot cover is
    /// answered by the log: the walk restarts at the start of the history,
    /// which answers a cursor below the window with the whole history rather
    /// than its newest part, and the payloads only memory holds are kept aside
    /// before that walk, because sequencing a log longer than the window fills
    /// it and would drop them before anything read them.
    ///
    /// A client with no cursor has seen nothing, and its walk starts at
    /// fresh_base: the ordinal of the oldest durable record the session holds
    /// in memory. The records below that ordinal stay reachable to the client
    /// through its own read of the log, so handing them over here would send it
    /// rows it releases as they arrive. None starts the walk at the head of the
    /// history, which is where a start nothing can name has to begin.
    pub(crate) fn begin_replay(
        &self,
        last: Option<EventSeq>,
        fresh_base: Option<usize>,
    ) -> ReplayPlan {
        let mut state = self.inner.state.lock().expect("event sequencer lock");
        if state.window.covers(last) {
            let opening = Self::collect_reliable_after(&state, last);
            return ReplayPlan {
                opening: Some(opening),
                held: Vec::new(),
            };
        }
        let mut held = state.window.runtime_payloads();
        held.extend(
            state
                .latest_status
                .values()
                .map(|frame| frame.payload.clone()),
        );
        state.trajectory_position = match (last, fresh_base) {
            (None, Some(index)) => TrajectoryPosition {
                index,
                byte_offset: None,
            },
            _ => TrajectoryPosition::start(),
        };
        // The window is emptied before the log is read: those frames are about
        // to be produced again under new sequence numbers, and the copies would
        // otherwise sit beside each other.
        state.window.clear();
        ReplayPlan {
            opening: None,
            held,
        }
    }

    /// Sequence one bounded batch of the log for a replay in progress, then the
    /// payloads memory held once the log's last batch has been read. Reports
    /// whether the replay is complete.
    pub(crate) fn next_replay_batch<F>(
        &self,
        plan: &mut ReplayPlan,
        load_durable_events: F,
    ) -> (Vec<EventEnvelope>, bool)
    where
        F: FnOnce(TrajectoryPosition) -> DurableBatch,
    {
        let mut state = self.inner.state.lock().expect("event sequencer lock");
        let mut frames = plan.opening.take().unwrap_or_default();
        let batch = load_durable_events(state.trajectory_position);
        let DurableBatch {
            entries,
            next,
            walk_finished,
        } = batch;
        frames.extend(Self::sequence_trajectory(&mut state, entries));
        state.trajectory_position = next;
        if !walk_finished {
            return (frames, false);
        }
        for payload in plan.held.drain(..) {
            frames.push(Self::sequence_runtime(&mut state, payload));
        }
        (frames, true)
    }

    /// Sequence the entries of one durable batch. The read position is the
    /// batch's to report, because a read can pass over records it cannot hold
    /// and only the walk knows how far it moved.
    fn sequence_trajectory(
        state: &mut SequencerState,
        entries: Vec<EntryPayloads>,
    ) -> Vec<EventEnvelope> {
        let mut sequenced = Vec::new();
        for entry in entries {
            for payload in entry.payloads {
                let frame = EventEnvelope::durable(
                    EventSeq(state.next_seq),
                    payload,
                    entry.event_id.clone(),
                );
                state.next_seq = state.next_seq.saturating_add(1);
                state.window.retain(frame.clone());
                sequenced.push(frame);
            }
        }
        sequenced
    }

    /// Give one runtime payload its sequence number and file it where its
    /// content belongs: a status frame supersedes that agent's previous status,
    /// and every other reliable frame joins the retained window.
    fn sequence_runtime(state: &mut SequencerState, payload: FrontendEvent) -> EventEnvelope {
        let frame = EventEnvelope::new(EventSeq(state.next_seq), payload);
        state.next_seq = state.next_seq.saturating_add(1);
        if let Some(agent_id) = agent_status_id(&frame.payload) {
            state
                .latest_status
                .insert(agent_id.to_string(), frame.clone());
        } else {
            state.window.retain(frame.clone());
        }
        frame
    }

    fn sequence_pending(state: &mut SequencerState) -> Vec<EventEnvelope> {
        let mut pending: Vec<_> = state.pending.drain(..).collect();
        pending.extend(state.pending_status.drain().map(|(_, event)| event));
        pending.sort_unstable_by_key(|event| event.order);
        state.ephemeral_pending = 0;
        let mut sequenced = Vec::with_capacity(pending.len());
        for event in pending {
            // An ephemeral preview is sequenced for delivery and released with
            // it, so only a reliable payload is filed for a later reconnect.
            let frame = if event.reliable {
                Self::sequence_runtime(state, event.payload)
            } else {
                let frame = EventEnvelope::new(EventSeq(state.next_seq), event.payload);
                state.next_seq = state.next_seq.saturating_add(1);
                frame
            };
            sequenced.push(frame);
        }
        sequenced
    }

    #[cfg(test)]
    pub(crate) fn sequence_pending_for_test(&self) -> Vec<EventEnvelope> {
        let mut state = self.inner.state.lock().expect("event sequencer lock");
        Self::sequence_pending(&mut state)
    }

    pub(crate) fn sequence_reliable(&self, payload: FrontendEvent) -> EventEnvelope {
        let mut state = self.inner.state.lock().expect("event sequencer lock");
        Self::sequence_runtime(&mut state, payload)
    }

    fn collect_reliable_after(
        state: &SequencerState,
        last: Option<EventSeq>,
    ) -> Vec<EventEnvelope> {
        let mut frames = state.window.after(last);
        frames.extend(
            state
                .latest_status
                .values()
                .filter(|frame| after_cursor(frame.seq, last))
                .cloned(),
        );
        frames.sort_unstable_by_key(|frame| frame.seq);
        frames
    }

    pub(crate) fn reliable_after(&self, last: Option<EventSeq>) -> Vec<EventEnvelope> {
        let state = self.inner.state.lock().expect("event sequencer lock");
        Self::collect_reliable_after(&state, last)
    }

    #[cfg(test)]
    pub(crate) fn next_seq(&self) -> u64 {
        self.inner
            .state
            .lock()
            .expect("event sequencer lock")
            .next_seq
    }
}

#[cfg(test)]
mod tests;
