//! The bus bridge: a LiveSink that forwards each turn-boundary snapshot onto
//! the multi-agent bus as a Progress message. Installed as a spawned child's
//! live sink so a parent watching the progress topic sees the child advance
//! per turn. Reuses the runner's live-sink seam rather than a parallel
//! turn-progress field, so the runner grows no second notification field.

use std::sync::Arc;

use houyicoder_api::live::{LiveEvent, LiveSink};

use super::bus_types::{AgentBus, BusMessage, ChildStatus, completed_topic, progress_topic};
use houyicoder_async::bus::MessageBus;

/// Map the runner's coarse terminal status string to the bus ChildStatus.
fn child_status_of(status: &str) -> ChildStatus {
    match status {
        "completed" | "handoff" => ChildStatus::Completed,
        "failed" | "verify_failed" => ChildStatus::Failed,
        "interrupted" => ChildStatus::Killed,
        "max_turns" => ChildStatus::DeadlineExceeded,
        _ => ChildStatus::Failed,
    }
}

/// Build a live sink that forwards turn-boundary snapshots + terminal
/// completion onto the child's bus topics. The agent_id is the child session
/// id; other LiveEvent variants are ignored — the bridge carries turn-level
/// progress + completion, not token deltas.
pub fn bus_live_sink(bus: Arc<AgentBus>, agent_id: String) -> LiveSink {
    Arc::new(move |ev: &LiveEvent| match ev {
        LiveEvent::TurnBoundary {
            turn,
            cumulative_tokens,
            tool_uses,
            last_activity,
        } => bus.publish(
            &progress_topic(&agent_id),
            BusMessage::Progress {
                agent_id: agent_id.clone(),
                turn: *turn,
                tokens: *cumulative_tokens,
                tool_uses: *tool_uses,
                last_activity: last_activity.clone(),
            },
        ),
        LiveEvent::RunCompleted { status, summary } => bus.publish(
            &completed_topic(&agent_id),
            BusMessage::Completed {
                agent_id: agent_id.clone(),
                status: child_status_of(status),
                summary: summary.clone(),
            },
        ),
        _ => {}
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

    /// A RunCompleted lands on the child's completed topic with the status
    /// mapped to ChildStatus + the summary carried through.
    #[test]
    fn test_sink_publishes_completion() {
        let bus = AgentBus::new();
        let mut rx = bus.subscribe(&completed_topic("child-4"));
        let sink = bus_live_sink(Arc::new(bus), "child-4".into());
        sink(&LiveEvent::RunCompleted {
            status: "completed".into(),
            summary: "found auth module".into(),
        });
        match rx.try_recv().expect("completion received") {
            BusMessage::Completed {
                agent_id,
                status,
                summary,
            } => {
                assert_eq!(agent_id, "child-4");
                assert_eq!(status, ChildStatus::Completed);
                assert_eq!(summary, "found auth module");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// A failed run maps to ChildStatus::Failed.
    #[test]
    fn test_sink_completion_failed() {
        let bus = AgentBus::new();
        let mut rx = bus.subscribe(&completed_topic("child-5"));
        let sink = bus_live_sink(Arc::new(bus), "child-5".into());
        sink(&LiveEvent::RunCompleted {
            status: "failed".into(),
            summary: "provider fatal".into(),
        });
        match rx.try_recv().expect("completion received") {
            BusMessage::Completed { status, .. } => {
                assert_eq!(status, ChildStatus::Failed);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}
