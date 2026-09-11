//! Session-scoped ordering for frontend events.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use houyicoder_api::agent_event::{
    self, AgentEventHandlers, EventHandler, MemoryChangedEvent, ResponseStreamEvent,
    ToolExecutionEvent, UserNoticeEvent,
};
use houyicoder_core::agent::Runner;
use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
use houyicoder_protocol::envelope::{EventEnvelope, EventSeq};
use houyicoder_protocol::frontend::FrontendEvent;
use houyicoder_protocol::frontend::memory::{
    MemoryChange, MemoryChangeId, MemoryChangeOrigin, MemoryOperation,
};
use tokio::sync::Notify;

const MAX_EPHEMERAL_PENDING: usize = 256;

#[derive(Clone)]
struct PendingEvent {
    order: u64,
    payload: FrontendEvent,
    reliable: bool,
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
        self.enqueue_reliable(FrontendEvent::MemoryChanged {
            id: MemoryChangeId(event.id.to_string()),
            origin: to_protocol_memory_origin(event.origin),
            changes: event
                .changes
                .into_iter()
                .map(|change| MemoryChange {
                    key: change.key,
                    operation: to_protocol_memory_operation(change.operation),
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

fn to_protocol_memory_operation(operation: agent_event::MemoryOperation) -> MemoryOperation {
    match operation {
        agent_event::MemoryOperation::Stored => MemoryOperation::Stored,
        agent_event::MemoryOperation::Deleted => MemoryOperation::Deleted,
        agent_event::MemoryOperation::Promoted => MemoryOperation::Promoted,
        agent_event::MemoryOperation::Demoted => MemoryOperation::Demoted,
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

    pub(crate) fn reset_projection(&self) {
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
        F: FnOnce(usize) -> Vec<Vec<FrontendEvent>>,
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
        F: FnOnce(usize) -> Vec<Vec<FrontendEvent>>,
    {
        let mut state = self.inner.state.lock().expect("event sequencer lock");
        let durable_events = load_durable_events(state.trajectory_cursor);
        drop(Self::sequence_trajectory(&mut state, durable_events));
        Self::collect_reliable_after(&state, last)
    }

    fn sequence_trajectory(
        state: &mut SequencerState,
        entries: Vec<Vec<FrontendEvent>>,
    ) -> Vec<EventEnvelope> {
        let mut sequenced = Vec::new();
        for events in entries {
            for payload in events {
                let frame = EventEnvelope::new(EventSeq(state.next_seq), payload);
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
mod tests {
    use super::*;
    use houyicoder_context as context;

    fn line(text: &str) -> FrontendEvent {
        FrontendEvent::SystemLine {
            text: text.to_string(),
        }
    }

    fn status(agent_id: &str, turn: u32) -> FrontendEvent {
        FrontendEvent::AgentStatus {
            agent_id: agent_id.to_string(),
            subagent_type: "explore".to_string(),
            turn,
            tokens: 0,
            tool_uses: 0,
            last_activity: None,
            completed: None,
        }
    }

    #[test]
    fn test_durable_precedes_preview() {
        let sequencer = EventSequencer::new();
        sequencer.enqueue_ephemeral(line("delta"));

        let frames = sequencer.sequence_available(|_| vec![vec![line("user"), line("commit")]]);

        assert_eq!(frames[0].seq, EventSeq(0));
        assert_eq!(frames[1].seq, EventSeq(1));
        assert_eq!(frames[2].seq, EventSeq(2));
    }

    #[test]
    fn test_replay_resumes_after_seq() {
        let sequencer = EventSequencer::new();
        let first = sequencer.sequence_reliable(line("first"));
        let second = sequencer.sequence_reliable(line("second"));
        let replay = sequencer.reliable_after(Some(first.seq));

        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].seq, second.seq);
    }

    #[test]
    fn test_memory_event_projects_fields() {
        let sequencer = EventSequencer::new();
        let id = context::MemoryChangeId::new();
        EventHandler::<MemoryChangedEvent>::handle(
            &sequencer,
            MemoryChangedEvent {
                id,
                origin: agent_event::MemoryChangeOrigin::AutoDream,
                changes: vec![agent_event::MemoryChange {
                    key: "build-gate".into(),
                    operation: agent_event::MemoryOperation::Promoted,
                }],
            },
        );
        let pending = sequencer.sequence_pending_for_test();
        assert!(matches!(
            &pending[0].payload,
            FrontendEvent::MemoryChanged { id: projected, origin: MemoryChangeOrigin::AutoDream, changes }
                if projected.0 == id.to_string()
                    && changes[0].operation == MemoryOperation::Promoted
        ));
    }

    #[test]
    fn test_tool_event_projects_progress() {
        let sequencer = EventSequencer::new();
        EventHandler::<ToolExecutionEvent>::handle(
            &sequencer,
            ToolExecutionEvent::Progress {
                call_id: "call-1".into(),
                elapsed_secs: 3,
                output_lines: Some(7),
            },
        );
        let pending = sequencer.sequence_pending_for_test();
        assert!(matches!(
            &pending[0].payload,
            FrontendEvent::Acpx { notification }
                if notification.method == AcpxMethod::ToolProgress
        ));
    }

    #[test]
    fn test_notice_event_projects_line() {
        let sequencer = EventSequencer::new();
        EventHandler::<UserNoticeEvent>::handle(
            &sequencer,
            UserNoticeEvent {
                message: "check config".into(),
            },
        );
        let pending = sequencer.sequence_pending_for_test();
        assert!(matches!(
            &pending[0].payload,
            FrontendEvent::SystemLine { text } if text == "check config"
        ));
    }

    #[test]
    fn test_runner_events_defer_sequence() {
        let sequencer = EventSequencer::new();
        EventHandler::<ResponseStreamEvent>::handle(
            &sequencer,
            ResponseStreamEvent::AssistantTextDelta {
                text: "next".into(),
            },
        );

        assert_eq!(sequencer.next_seq(), 0);
        let pending = sequencer.sequence_pending_for_test();
        assert_eq!(pending[0].seq, EventSeq(0));
    }

    #[test]
    fn test_status_replay_keeps_latest() {
        let sequencer = EventSequencer::new();
        sequencer.enqueue_reliable(status("child", 1));
        sequencer.enqueue_reliable(status("child", 2));
        let pending = sequencer.sequence_pending_for_test();
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            pending[0].payload,
            FrontendEvent::AgentStatus { turn: 2, .. }
        ));

        sequencer.enqueue_reliable(status("child", 3));
        drop(sequencer.sequence_pending_for_test());
        let replay = sequencer.reliable_after(None);
        assert_eq!(replay.len(), 1);
        assert!(matches!(
            replay[0].payload,
            FrontendEvent::AgentStatus { turn: 3, .. }
        ));
    }

    #[test]
    fn test_status_coalescing_preserves_order() {
        let sequencer = EventSequencer::new();
        sequencer.enqueue_reliable(status("first", 1));
        sequencer.enqueue_reliable(line("middle"));
        sequencer.enqueue_reliable(status("first", 2));
        sequencer.enqueue_reliable(status("second", 1));

        let pending = sequencer.sequence_pending_for_test();
        assert!(matches!(
            &pending[0].payload,
            FrontendEvent::SystemLine { text } if text == "middle"
        ));
        assert!(matches!(
            &pending[1].payload,
            FrontendEvent::AgentStatus { agent_id, turn: 2, .. } if agent_id == "first"
        ));
        assert!(matches!(
            &pending[2].payload,
            FrontendEvent::AgentStatus { agent_id, turn: 1, .. } if agent_id == "second"
        ));
    }

    #[test]
    fn test_snapshot_blocks_causal_delta() {
        let sequencer = EventSequencer::new();
        let sequencer_for_load = sequencer.clone();
        let (snapshot_tx, snapshot_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let sequencing = std::thread::spawn(move || {
            sequencer_for_load.sequence_available(|_| {
                snapshot_tx.send(()).expect("snapshot entered");
                release_rx.recv().expect("snapshot released");
                vec![vec![line("user"), line("commit")]]
            })
        });
        snapshot_rx.recv().expect("snapshot started");

        let producer = sequencer.clone();
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let enqueue = std::thread::spawn(move || {
            attempt_tx.send(()).expect("enqueue attempted");
            producer.enqueue_ephemeral(line("delta"));
        });
        attempt_rx.recv().expect("producer started");
        release_tx.send(()).expect("release snapshot");

        let durable = sequencing.join().expect("sequencing thread");
        enqueue.join().expect("producer thread");
        let pending = sequencer.sequence_pending_for_test();
        assert_eq!(durable[0].seq, EventSeq(0));
        assert_eq!(durable[1].seq, EventSeq(1));
        assert_eq!(pending[0].seq, EventSeq(2));
    }
}
