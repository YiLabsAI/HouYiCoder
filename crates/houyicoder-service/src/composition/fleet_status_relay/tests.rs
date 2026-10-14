//! Tests for this module.

use super::*;
use houyicoder_async::CancellationToken;
use houyicoder_core::agent::multi_agent::bus_types::{ChildDescriptor, ChildRunMode};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::time::timeout;

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
        Handle::current(),
        CancellationToken::new(),
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
    timeout(Duration::from_secs(1), sequencer.notified())
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
    timeout(Duration::from_secs(1), sequencer.notified())
        .await
        .expect("completion status submitted");
    let frames = sequencer.sequence_pending_for_test();
    assert!(matches!(
        &frames[0].payload,
        FrontendEvent::AgentStatus { completed: Some(status), .. } if status == "completed"
    ));
}

/// The cancel token ends the relay loop: a host shutdown cancels the
/// token, the relay's select branch fires, and the task exits. Pairs
/// with the drain's cancel contract so a session switch reaps both.
#[tokio::test]
async fn test_cancel_ends_relay() {
    let bus = Arc::new(AgentBus::new());
    let sequencer = EventSequencer::new();
    let cancel = CancellationToken::new();
    let handle = spawn(
        Some(bus.clone()),
        sequencer,
        Handle::current(),
        cancel.clone(),
    )
    .expect("a bus present yields a handle");
    cancel.cancel();
    let outcome = timeout(Duration::from_secs(1), handle).await;
    assert!(outcome.is_ok(), "cancel ends the relay task");
    assert!(
        outcome.unwrap().is_ok(),
        "the relay task exited without panic"
    );
}
