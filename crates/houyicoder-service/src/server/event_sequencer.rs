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

#[derive(Default)]
struct SequencerState {
    next_seq: u64,
    next_pending_order: u64,
    trajectory_cursor: usize,
    pending: VecDeque<PendingEvent>,
    pending_status: HashMap<String, PendingEvent>,
    ephemeral_pending: usize,
    reliable: Vec<EventEnvelope>,
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

/// The per-session ordering authority for frontend events.
///
/// Producers enqueue typed events without assigning sequence numbers. The
/// active Server is the sole consumer and frontend event writer, so sequence
/// allocation and carrier order cannot diverge. Reliable frames remain in
/// memory for a
/// reconnect to the same server process; ephemeral previews may be dropped
/// while their queue is full.
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
        Self {
            inner: Arc::new(SequencerInner {
                state: Mutex::new(SequencerState::default()),
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
        state.reliable.clear();
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
        let durable_events = load_durable_events(state.trajectory_cursor);
        drop(Self::sequence_trajectory(&mut state, durable_events));
        Self::collect_reliable_after(&state, last)
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
                state.reliable.push(frame.clone());
                sequenced.push(frame);
            }
            state.trajectory_cursor = state.trajectory_cursor.saturating_add(1);
        }
        sequenced
    }

    fn sequence_pending(state: &mut SequencerState) -> Vec<EventEnvelope> {
        let mut pending: Vec<_> = state.pending.drain(..).collect();
        pending.extend(state.pending_status.drain().map(|(_, event)| event));
        pending.sort_unstable_by_key(|event| event.order);
        state.ephemeral_pending = 0;
        let mut sequenced = Vec::with_capacity(pending.len());
        for event in pending {
            let frame = EventEnvelope::new(EventSeq(state.next_seq), event.payload);
            state.next_seq = state.next_seq.saturating_add(1);
            if event.reliable {
                if let Some(agent_id) = agent_status_id(&frame.payload) {
                    state
                        .latest_status
                        .insert(agent_id.to_string(), frame.clone());
                } else {
                    state.reliable.push(frame.clone());
                }
            }
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
        let frame = EventEnvelope::new(EventSeq(state.next_seq), payload);
        state.next_seq = state.next_seq.saturating_add(1);
        if let Some(agent_id) = agent_status_id(&frame.payload) {
            state
                .latest_status
                .insert(agent_id.to_string(), frame.clone());
        } else {
            state.reliable.push(frame.clone());
        }
        frame
    }

    fn collect_reliable_after(
        state: &SequencerState,
        last: Option<EventSeq>,
    ) -> Vec<EventEnvelope> {
        let mut frames: Vec<_> = state
            .reliable
            .iter()
            .chain(state.latest_status.values())
            .filter(|frame| last.is_none_or(|seq| frame.seq > seq))
            .cloned()
            .collect();
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
