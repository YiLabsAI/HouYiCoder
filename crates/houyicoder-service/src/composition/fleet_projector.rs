//! The fleet projector: a service-side bridge that subscribes to the
//! multi-agent bus and translates child progress and completion into
//! FrontendEventKind::AgentStatus wire frames the TUI renders as the agent
//! status footer. Lives in the service so the TUI (presentation) stays free
//! of the engine bus dependency.
//!
//! Event-driven: the main task awaits the spawned topic; each new child
//! gets its own drain task that awaits that child's progress and completed
//! topics and emits a wire frame per message. No polling.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::channel::mpsc;

use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, ChildStatus, completed_topic, progress_topic, spawned_topic,
};
use houyicoder_protocol::envelope::{EventEnvelope, EventSeq, ServerFrame};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::event_kind::FrontendEventKind;

/// Spawn the fleet projector on the runtime. It subscribes to the spawned
/// topic and, per child, drains progress + completed into wire frames on
/// the same outbound channel the live delta sink uses. No-op when the bus
/// is absent (non-multi-agent runs).
pub fn spawn(
    bus: Option<Arc<AgentBus>>,
    out_tx: mpsc::Sender<String>,
    next_seq: Arc<AtomicU64>,
    runtime: tokio::runtime::Handle,
) {
    let Some(bus) = bus else {
        return;
    };
    let mut spawned_rx = bus.subscribe(spawned_topic());
    let inner_runtime = runtime.clone();
    runtime.spawn(async move {
        loop {
            if let Ok(BusMessage::Spawned {
                agent_id,
                subagent_type,
            }) = spawned_rx.recv().await
            {
                let progress_rx = bus.subscribe(&progress_topic(&agent_id));
                let completed_rx = bus.subscribe(&completed_topic(&agent_id));
                inner_runtime.spawn(drain_child(
                    agent_id,
                    subagent_type,
                    progress_rx,
                    completed_rx,
                    out_tx.clone(),
                    next_seq.clone(),
                ));
            }
        }
    });
}

/// Drain one child's progress + completed topics, emitting an AgentStatus
/// wire frame per message. Exits when the child completes (Completed
/// received).
async fn drain_child(
    agent_id: String,
    subagent_type: String,
    mut progress_rx: tokio::sync::broadcast::Receiver<BusMessage>,
    mut completed_rx: tokio::sync::broadcast::Receiver<BusMessage>,
    mut out_tx: mpsc::Sender<String>,
    next_seq: Arc<AtomicU64>,
) {
    let mut last = Snapshot {
        turn: 0,
        tokens: 0,
        tool_uses: 0,
        last_activity: None,
        completed: None,
    };
    loop {
        tokio::select! {
            msg = progress_rx.recv() => {
                if let Ok(BusMessage::Progress { turn, tokens, tool_uses, last_activity, .. }) = msg {
                    last = Snapshot { turn, tokens, tool_uses, last_activity, completed: None };
                    emit(&mut out_tx, &next_seq, &agent_id, &subagent_type, &last);
                }
            }
            msg = completed_rx.recv() => {
                if let Ok(BusMessage::Completed { status, summary, .. }) = msg {
                    last.completed = Some(status_str(&status));
                    last.last_activity = Some(summary);
                    emit(&mut out_tx, &next_seq, &agent_id, &subagent_type, &last);
                    return;
                }
            }
            else => { return; }
        }
    }
}

struct Snapshot {
    turn: u32,
    tokens: u64,
    tool_uses: u32,
    last_activity: Option<String>,
    completed: Option<String>,
}

fn status_str(status: &ChildStatus) -> String {
    match status {
        ChildStatus::Completed => "completed",
        ChildStatus::Killed => "killed",
        ChildStatus::Failed => "failed",
        ChildStatus::DeadlineExceeded => "deadline",
        ChildStatus::BudgetExhausted => "budget",
    }
    .to_string()
}

fn emit(
    out_tx: &mut mpsc::Sender<String>,
    next_seq: &Arc<AtomicU64>,
    agent_id: &str,
    subagent_type: &str,
    snap: &Snapshot,
) {
    let seq = next_seq.fetch_add(1, Ordering::Relaxed);
    let frame = ServerFrame::Event(EventEnvelope::new(
        EventSeq(seq),
        FrontendEventKind::AgentStatus {
            agent_id: agent_id.to_string(),
            subagent_type: subagent_type.to_string(),
            turn: snap.turn,
            tokens: snap.tokens,
            tool_uses: snap.tool_uses,
            last_activity: snap.last_activity.clone(),
            completed: snap.completed.clone(),
        },
    ));
    if let Ok(line) = encode(&frame) {
        let _send = out_tx.try_send(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use houyicoder_protocol::envelope::ServerFrame;
    use houyicoder_protocol::frontend::event_kind::FrontendEventKind;

    #[tokio::test]
    async fn test_projector_translates_progress() {
        let bus = Arc::new(AgentBus::new());
        let (tx, mut rx) = mpsc::channel(16);
        let next_seq = Arc::new(AtomicU64::new(0));
        spawn(
            Some(bus.clone()),
            tx,
            next_seq,
            tokio::runtime::Handle::current(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                agent_id: "c1".into(),
                subagent_type: "explore".into(),
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bus.publish(
            &progress_topic("c1"),
            BusMessage::Progress {
                agent_id: "c1".into(),
                turn: 1,
                tokens: 100,
                tool_uses: 2,
                last_activity: Some("grep".into()),
            },
        );
        let line = rx.next().await.expect("wire frame received");
        let frame: ServerFrame = serde_json::from_str(&line).unwrap();
        match frame {
            ServerFrame::Event(ev) => match ev.payload {
                FrontendEventKind::AgentStatus { agent_id, turn, .. } => {
                    assert_eq!(agent_id, "c1");
                    assert_eq!(turn, 1);
                }
                other => panic!("expected AgentStatus, got {other:?}"),
            },
            other => panic!("expected Event, got {other:?}"),
        }
    }
}
