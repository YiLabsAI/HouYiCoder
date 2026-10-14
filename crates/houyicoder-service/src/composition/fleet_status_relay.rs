//! Translates multi-agent status changes into sequenced frontend events.

use std::collections::HashMap;
use std::sync::Arc;

use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, ChildStatus, global_completed_topic, global_progress_topic, spawned_topic,
};
use houyicoder_protocol::frontend::event::FrontendEvent;

use crate::server::EventSequencer;
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

/// Subscribe to fleet status changes and submit reliable status events.
/// Returns the task handle for the host lifecycle owner to reap; the cancel
/// token ends the loop at shutdown so the relay does not outlive its bus.
pub fn spawn(
    bus: Option<Arc<AgentBus>>,
    event_sequencer: EventSequencer,
    runtime: Handle,
    cancel: houyicoder_async::CancellationToken,
) -> Option<JoinHandle<()>> {
    let bus = bus?;
    let mut spawned_rx = bus.subscribe(spawned_topic());
    let mut progress_rx = bus.subscribe(global_progress_topic());
    let mut completed_rx = bus.subscribe(global_completed_topic());
    let handle = runtime.spawn(async move {
        let mut children: HashMap<String, (String, Snapshot)> = HashMap::new();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                msg = spawned_rx.recv() => match msg {
                    Ok(BusMessage::Spawned { child }) => {
                        let snap = Snapshot::default();
                        emit(&event_sequencer, &child.agent_id, &child.agent_type, &snap);
                        children.insert(child.agent_id, (child.agent_type, snap));
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
                msg = progress_rx.recv() => match msg {
                    Ok(BusMessage::Progress { child, turn, tokens, tool_uses, last_activity }) => {
                        let entry = children
                            .entry(child.agent_id.clone())
                            .or_insert_with(|| (child.agent_type, Snapshot::default()));
                        entry.1 = Snapshot { turn, tokens, tool_uses, last_activity, completed: None };
                        emit(&event_sequencer, &child.agent_id, &entry.0, &entry.1);
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
                msg = completed_rx.recv() => match msg {
                    Ok(BusMessage::Completed { child, status, summary }) => {
                        let mut snap = children
                            .remove(&child.agent_id)
                            .map_or_else(Snapshot::default, |(_, snap)| snap);
                        snap.completed = Some(status_str(&status));
                        snap.last_activity = Some(summary);
                        emit(&event_sequencer, &child.agent_id, &child.agent_type, &snap);
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
            }
        }
    });
    Some(handle)
}

#[derive(Default)]
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
        ChildStatus::TurnLimit => "turn_limit",
        ChildStatus::BudgetExhausted => "budget",
    }
    .to_string()
}

fn emit(event_sequencer: &EventSequencer, agent_id: &str, subagent_type: &str, snap: &Snapshot) {
    event_sequencer.enqueue_reliable(FrontendEvent::AgentStatus {
        agent_id: agent_id.to_string(),
        subagent_type: subagent_type.to_string(),
        turn: snap.turn,
        tokens: snap.tokens,
        tool_uses: snap.tool_uses,
        last_activity: snap.last_activity.clone(),
        completed: snap.completed.clone(),
    });
}

#[cfg(test)]
mod tests;
