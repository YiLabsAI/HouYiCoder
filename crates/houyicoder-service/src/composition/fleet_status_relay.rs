//! Translates multi-agent status changes into sequenced frontend events.

use std::collections::HashMap;
use std::sync::Arc;

use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, ChildStatus, global_completed_topic, global_progress_topic, spawned_topic,
};
use houyicoder_protocol::frontend::event::FrontendEvent;

use crate::server::EventSequencer;

/// Subscribe to fleet status changes and submit reliable status events.
pub fn spawn(
    bus: Option<Arc<AgentBus>>,
    event_sequencer: EventSequencer,
    runtime: tokio::runtime::Handle,
) {
    let Some(bus) = bus else {
        return;
    };
    let mut spawned_rx = bus.subscribe(spawned_topic());
    let mut progress_rx = bus.subscribe(global_progress_topic());
    let mut completed_rx = bus.subscribe(global_completed_topic());
    runtime.spawn(async move {
        let mut children: HashMap<String, (String, Snapshot)> = HashMap::new();
        loop {
            tokio::select! {
                biased;
                msg = spawned_rx.recv() => match msg {
                    Ok(BusMessage::Spawned { child }) => {
                        let snap = Snapshot::default();
                        emit(&event_sequencer, &child.agent_id, &child.agent_type, &snap);
                        children.insert(child.agent_id, (child.agent_type, snap));
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
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
mod tests {
    use super::*;
    use houyicoder_core::agent::multi_agent::bus_types::{ChildDescriptor, ChildRunMode};

    #[test]
    fn test_emit_submits_reliable_status() {
        let sequencer = EventSequencer::new();
        emit(
            &sequencer,
            "c1",
            "explore",
            &Snapshot {
                turn: 2,
                tokens: 50,
                tool_uses: 1,
                last_activity: Some("read".into()),
                completed: None,
            },
        );
        let frames = sequencer.sequence_pending_for_test();
        assert!(matches!(
            &frames[0].payload,
            FrontendEvent::AgentStatus { agent_id, turn: 2, .. } if agent_id == "c1"
        ));
        assert_eq!(sequencer.reliable_after(None).len(), 1);
    }

    #[tokio::test]
    async fn test_spawn_translates_completion() {
        let bus = Arc::new(AgentBus::new());
        let sequencer = EventSequencer::new();
        spawn(
            Some(bus.clone()),
            sequencer.clone(),
            tokio::runtime::Handle::current(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                child: ChildDescriptor {
                    agent_id: "c1".into(),
                    agent_type: "explore".into(),
                    run_mode: ChildRunMode::Background,
                },
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), sequencer.notified())
            .await
            .expect("spawn status submitted");
        drop(sequencer.sequence_pending_for_test());
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                child: ChildDescriptor {
                    agent_id: "c1".into(),
                    agent_type: "explore".into(),
                    run_mode: ChildRunMode::Background,
                },
                status: ChildStatus::Completed,
                summary: "done".into(),
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), sequencer.notified())
            .await
            .expect("completion status submitted");
        let frames = sequencer.sequence_pending_for_test();
        assert!(matches!(
            &frames[0].payload,
            FrontendEvent::AgentStatus { completed: Some(status), .. } if status == "completed"
        ));
    }
}
