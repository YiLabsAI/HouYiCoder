//! Session-scoped ordering for frontend events.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use houyicoder_api::live::LiveEvent;
use houyicoder_core::agent::Runner;
use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
use houyicoder_protocol::envelope::{EventEnvelope, EventSeq};
use houyicoder_protocol::frontend::FrontendEvent;
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
/// active Server is the sole consumer and wire writer, so sequence allocation
/// and carrier order cannot diverge. Reliable frames remain journaled for a
/// reconnect; ephemeral previews may be dropped while their queue is full.
#[derive(Clone)]
pub struct EventSequencer {
    inner: Arc<SequencerInner>,
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
        let sequencer = self.clone();
        runner.set_live_sink(Arc::new(move |event| {
            sequencer.enqueue_runner_event(event);
        }));
    }

    fn enqueue_runner_event(&self, event: &LiveEvent) {
        match event {
            LiveEvent::AssistantDelta { text } => {
                self.enqueue_ephemeral(FrontendEvent::Acpx {
                    notification: AcpxNotification::new(
                        AcpxMethod::LlmTextDelta,
                        serde_json::json!({ "text": text }),
                    ),
                });
            }
            LiveEvent::ReasoningDelta { text } => {
                self.enqueue_ephemeral(FrontendEvent::Acpx {
                    notification: AcpxNotification::new(
                        AcpxMethod::LlmReasoningDelta,
                        serde_json::json!({ "text": text }),
                    ),
                });
            }
            LiveEvent::ToolProgress {
                call_id,
                elapsed_secs,
                lines,
            } => {
                self.enqueue_ephemeral(FrontendEvent::Acpx {
                    notification: AcpxNotification::new(
                        AcpxMethod::ToolProgress,
                        serde_json::json!({ "call_id": call_id, "elapsed_secs": elapsed_secs, "lines": lines }),
                    ),
                });
            }
            LiveEvent::MemorySaved { count, kind } => {
                self.enqueue_reliable(FrontendEvent::MemorySaved {
                    count: *count,
                    kind: *kind,
                });
            }
            LiveEvent::SystemLine { text } => {
                self.enqueue_reliable(FrontendEvent::SystemLine { text: text.clone() });
            }
            LiveEvent::TurnBoundary { .. } | LiveEvent::RunCompleted { .. } => {}
        }
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

    pub(crate) fn sequence_available<F>(&self, project: F) -> Vec<EventEnvelope>
    where
        F: FnOnce(usize) -> Vec<Vec<FrontendEvent>>,
    {
        let mut state = self.inner.state.lock().expect("event sequencer lock");
        let entries = project(state.trajectory_cursor);
        let mut sequenced = Self::sequence_trajectory(&mut state, entries);
        sequenced.extend(Self::sequence_pending(&mut state));
        sequenced
    }

    pub(crate) fn prepare_replay<F>(&self, last: Option<EventSeq>, project: F) -> Vec<EventEnvelope>
    where
        F: FnOnce(usize) -> Vec<Vec<FrontendEvent>>,
    {
        let mut state = self.inner.state.lock().expect("event sequencer lock");
        let entries = project(state.trajectory_cursor);
        drop(Self::sequence_trajectory(&mut state, entries));
        Self::collect_reliable_after(&state, last)
    }

    fn sequence_trajectory(
        state: &mut SequencerState,
        entries: Vec<Vec<FrontendEvent>>,
    ) -> Vec<EventEnvelope> {
        let mut sequenced = Vec::new();
        for projections in entries {
            for payload in projections {
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
    fn test_runner_events_defer_sequence() {
        let sequencer = EventSequencer::new();
        sequencer.enqueue_runner_event(&LiveEvent::AssistantDelta {
            text: "next".into(),
        });

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
        let projector = sequencer.clone();
        let (snapshot_tx, snapshot_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let projection = std::thread::spawn(move || {
            projector.sequence_available(|_| {
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

        let durable = projection.join().expect("projection thread");
        enqueue.join().expect("producer thread");
        let pending = sequencer.sequence_pending_for_test();
        assert_eq!(durable[0].seq, EventSeq(0));
        assert_eq!(durable[1].seq, EventSeq(1));
        assert_eq!(pending[0].seq, EventSeq(2));
    }
}
