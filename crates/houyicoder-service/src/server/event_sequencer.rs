//! Session-scoped ordering for frontend events.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use houyicoder_api::agent_event::{
    self, AgentEventHandlers, EventHandler, MemoryChangedEvent, ResponseStreamEvent,
    ToolExecutionEvent, UserNoticeEvent,
};
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
    trajectory_cursor: usize,
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
        state.trajectory_cursor = 0;
        state.next_pending_order = 0;
        state.pending.clear();
        state.pending_status.clear();
        state.ephemeral_pending = 0;
        state.window.clear();
        state.latest_status.clear();
    }

    pub(crate) fn sequence_available<F>(&self, load_durable_events: F) -> Vec<EventEnvelope>
    where
        F: FnOnce(usize) -> Vec<EntryPayloads>,
    {
        let mut state = self.inner.state.lock().expect("event sequencer lock");
        let durable_events = load_durable_events(state.trajectory_cursor);
        let mut sequenced = Self::sequence_trajectory(&mut state, durable_events);
        sequenced.extend(Self::sequence_pending(&mut state));
        sequenced
    }

    pub(crate) fn prepare_replay<F>(
        &self,
        last: Option<EventSeq>,
        load_durable_events: F,
    ) -> Vec<EventEnvelope>
    where
        F: FnOnce(usize) -> Vec<EntryPayloads>,
    {
        let mut state = self.inner.state.lock().expect("event sequencer lock");
        if !state.window.covers(last) {
            // The window cannot answer this cursor, so the log does. The frames
            // only memory holds are lifted out before that read, because
            // sequencing a log longer than the window fills it and would drop
            // them before anything read them. Reading the log from its start
            // rather than the window's tail also answers a cursor below the
            // window with the whole history instead of its newest part.
            let mut held = state.window.runtime_payloads();
            held.extend(
                state
                    .latest_status
                    .values()
                    .map(|frame| frame.payload.clone()),
            );
            state.trajectory_cursor = 0;
            // The window is emptied before the log is read: those frames are
            // about to be produced again under new sequence numbers, and the
            // copies would otherwise sit beside each other.
            state.window.clear();
            let mut replay = Self::sequence_trajectory(&mut state, load_durable_events(0));
            // Held frames are sequenced after the log so the client's cursor
            // still advances; each is filed again under its new sequence, which
            // is what keeps it reachable for the next reconnect.
            for payload in held {
                replay.push(Self::sequence_runtime(&mut state, payload));
            }
            return replay;
        }
        // The delta is taken before this round's load can trim the front of
        // the window, and the frames that load makes durable join it at the
        // tail.
        let mut replay = Self::collect_reliable_after(&state, last);
        let durable_events = load_durable_events(state.trajectory_cursor);
        replay.extend(Self::sequence_trajectory(&mut state, durable_events));
        replay
    }

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
            state.trajectory_cursor = state.trajectory_cursor.saturating_add(1);
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
