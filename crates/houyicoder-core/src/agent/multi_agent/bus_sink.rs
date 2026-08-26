//! The bus bridge: a LiveSink that forwards each turn-boundary snapshot onto
//! the multi-agent bus as a Progress message. Installed as a spawned child's
//! live sink so a parent watching the progress topic sees the child advance
//! per turn. Reuses the runner's live-sink seam rather than a parallel
//! turn-progress field, so the runner grows no second notification field.

use std::sync::Arc;

use houyicoder_api::live::{LiveEvent, LiveSink};

use super::bus_types::{AgentBus, BusMessage, progress_topic};
use houyicoder_async::bus::MessageBus;

/// Build a live sink that publishes each turn-boundary snapshot onto the
/// child's progress topic. The agent_id is the child session id (the topic is
/// task.<id>.progress); the bus is the shared in-process transport the parent
/// also subscribes to. Other LiveEvent variants pass through ignored — the
/// bridge carries turn-level progress, not token deltas.
pub fn bus_live_sink(bus: Arc<AgentBus>, agent_id: String) -> LiveSink {
    Arc::new(move |ev: &LiveEvent| {
        if let LiveEvent::TurnBoundary {
            turn,
            cumulative_tokens,
            tool_uses,
            last_activity,
        } = ev
        {
            bus.publish(
                &progress_topic(&agent_id),
                BusMessage::Progress {
                    agent_id: agent_id.clone(),
                    turn: *turn,
                    tokens: *cumulative_tokens,
                    tool_uses: *tool_uses,
                    last_activity: last_activity.clone(),
                },
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_async::bus::MessageBus;

    /// A TurnBoundary published through the sink lands on the child's progress
    /// topic with every field intact — the causal path the acceptance pins:
    /// publish → receive.
    #[test]
    fn test_sink_publishes_progress() {
        let bus = AgentBus::new();
        let mut rx = bus.subscribe(&progress_topic("child-1"));
        let sink = bus_live_sink(Arc::new(bus), "child-1".into());
        sink(&LiveEvent::TurnBoundary {
            turn: 3,
            cumulative_tokens: 1200,
            tool_uses: 2,
            last_activity: Some("grep".into()),
        });
        match rx.try_recv().expect("progress received") {
            BusMessage::Progress {
                agent_id,
                turn,
                tokens,
                tool_uses,
                last_activity,
            } => {
                assert_eq!(agent_id, "child-1");
                assert_eq!(turn, 3);
                assert_eq!(tokens, 1200);
                assert_eq!(tool_uses, 2);
                assert_eq!(last_activity.as_deref(), Some("grep"));
            }
            other => panic!("expected Progress, got {other:?}"),
        }
    }

    /// A text-only turn (no tool calls) publishes with last_activity=None and
    /// tool_uses=0, so a watcher renders the turn with no tool verb.
    #[test]
    fn test_sink_text_only_turn() {
        let bus = AgentBus::new();
        let mut rx = bus.subscribe(&progress_topic("child-2"));
        let sink = bus_live_sink(Arc::new(bus), "child-2".into());
        sink(&LiveEvent::TurnBoundary {
            turn: 1,
            cumulative_tokens: 50,
            tool_uses: 0,
            last_activity: None,
        });
        match rx.try_recv().expect("progress received") {
            BusMessage::Progress {
                tool_uses,
                last_activity,
                ..
            } => {
                assert_eq!(tool_uses, 0);
                assert!(last_activity.is_none());
            }
            other => panic!("expected Progress, got {other:?}"),
        }
    }

    /// Token deltas and other LiveEvent variants are ignored: the bus carries
    /// turn-level progress only, not the token stream. A child that streams
    /// deltas must not flood the bus.
    #[test]
    fn test_sink_ignores_non_boundary() {
        let bus = AgentBus::new();
        let mut rx = bus.subscribe(&progress_topic("child-3"));
        let sink = bus_live_sink(Arc::new(bus), "child-3".into());
        sink(&LiveEvent::AssistantDelta {
            text: "hello".into(),
        });
        sink(&LiveEvent::SystemLine {
            text: "notice".into(),
        });
        assert!(
            rx.try_recv().is_err(),
            "non-boundary events must be ignored"
        );
    }
}
