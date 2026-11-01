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
            description: String::new(),
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
                description: String::new(),
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
                description: String::new(),
                run_mode: ChildRunMode::Background,
            },
            status: CompletionStatus::Completed,
            summary: "done".into(),
        },
    );
    timeout(Duration::from_secs(1), sequencer.notified())
        .await
        .expect("completion status submitted");
    let frames = sequencer.sequence_pending_for_test();
    assert!(matches!(
        &frames[0].payload,
        FrontendEvent::AgentStatus { completed: Some(status), .. } if *status == CompletionStatus::Completed
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

/// The spawn status carries the delegating call's task summary, so the fleet
/// row names the job from the first frame. A descriptor built without it
/// leaves the row showing the type alone until the child finishes a turn,
/// which is exactly the stretch where a reader wants to know what it is doing.
#[tokio::test]
async fn test_spawn_status_carries_description() {
    let bus = Arc::new(AgentBus::new());
    let sequencer = EventSequencer::new();
    spawn(
        Some(Arc::clone(&bus)),
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
                description: "map the auth path".into(),
                run_mode: ChildRunMode::Background,
            },
        },
    );
    timeout(Duration::from_secs(1), sequencer.notified())
        .await
        .expect("spawn status submitted");
    let frames = sequencer.sequence_pending_for_test();
    let status = frames
        .iter()
        .find_map(|f| match &f.payload {
            FrontendEvent::AgentStatus { description, .. } => Some(description.clone()),
            _ => None,
        })
        .expect("a spawn status frame");
    assert_eq!(
        status, "map the auth path",
        "the first status names the job it is doing"
    );
}

/// A later status that carries no summary does not blank the label already
/// held. The relay is the only place that can keep it: the row's label would
/// otherwise disappear mid-run, when the child is busiest.
#[tokio::test]
async fn test_progress_keeps_description() {
    let bus = Arc::new(AgentBus::new());
    let sequencer = EventSequencer::new();
    spawn(
        Some(Arc::clone(&bus)),
        sequencer.clone(),
        Handle::current(),
        CancellationToken::new(),
    );
    let child = |description: &str| ChildDescriptor {
        agent_id: "c1".into(),
        agent_type: "explore".into(),
        description: description.into(),
        run_mode: ChildRunMode::Background,
    };
    bus.publish(
        spawned_topic(),
        BusMessage::Spawned {
            child: child("map the auth path"),
        },
    );
    timeout(Duration::from_secs(1), sequencer.notified())
        .await
        .expect("spawn status submitted");
    drop(sequencer.sequence_pending_for_test());

    bus.publish(
        global_progress_topic(),
        BusMessage::Progress {
            child: child(""),
            turn: 2,
            tokens: 10,
            tool_uses: 1,
            last_activity: Some("grep".into()),
        },
    );
    timeout(Duration::from_secs(1), sequencer.notified())
        .await
        .expect("progress status submitted");
    let frames = sequencer.sequence_pending_for_test();
    let status = frames
        .iter()
        .find_map(|f| match &f.payload {
            FrontendEvent::AgentStatus { description, .. } => Some(description.clone()),
            _ => None,
        })
        .expect("a progress status frame");
    assert_eq!(
        status, "map the auth path",
        "a status without a summary keeps the label already held"
    );
}
