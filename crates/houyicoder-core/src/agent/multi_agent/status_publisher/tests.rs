//! Tests for this module.

use super::super::bus_types::ChildRunMode;
use super::*;
use houyicoder_protocol::frontend::completion::CompletionStatus;

fn child(run_mode: ChildRunMode) -> ChildDescriptor {
    ChildDescriptor {
        agent_id: "child-1".into(),
        agent_type: "explore".into(),
        description: String::new(),
        run_mode,
    }
}

#[test]
fn test_publisher_sends_child_status() {
    for run_mode in [ChildRunMode::Foreground, ChildRunMode::Background] {
        let bus = Arc::new(AgentBus::new());
        let mut progress = bus.subscribe(global_progress_topic());
        let mut completed = bus.subscribe(global_completed_topic());
        let publisher = ChildStatusPublisher::new(Arc::clone(&bus), child(run_mode));
        publisher.handle(RunLifecycleEvent::TurnCompleted {
            turn: 2,
            cumulative_tokens: 300,
            tool_uses: 1,
            last_activity: Some("read".into()),
        });
        publisher.handle(RunLifecycleEvent::Completed {
            status: RunCompletionStatus::Completed,
            summary: "done".into(),
        });
        assert!(matches!(
            progress.try_recv().expect("progress"),
            BusMessage::Progress { child, turn: 2, .. } if child.run_mode == run_mode
        ));
        assert!(matches!(
            completed.try_recv().expect("completion"),
            BusMessage::Completed { child, status: CompletionStatus::Completed, .. }
                if child.run_mode == run_mode
        ));
    }
}

#[test]
fn test_completion_maps_run_outcome() {
    assert_eq!(
        reported_ending(RunCompletionStatus::HandedOff),
        CompletionStatus::Completed
    );
    assert_eq!(
        reported_ending(RunCompletionStatus::Interrupted),
        CompletionStatus::Killed
    );
    assert_eq!(
        reported_ending(RunCompletionStatus::VerificationFailed),
        CompletionStatus::Failed
    );
    assert_eq!(
        reported_ending(RunCompletionStatus::TurnLimitReached),
        CompletionStatus::TurnLimit
    );
}

/// A foreground child handed to the background while it runs must still
/// notify the parent. The mode is read when the ending is published, not when
/// the publisher was built, so the promotion reaches the notification.
#[test]
fn test_promoted_child_notifies() {
    let bus = Arc::new(AgentBus::new());
    let mut completed = bus.subscribe(global_completed_topic());
    let publisher = ChildStatusPublisher::new(Arc::clone(&bus), child(ChildRunMode::Foreground));
    // The reader promotes it mid-flight, exactly as the runtime does.
    publisher.backgrounded().store(true, Ordering::Release);
    publisher.handle(RunLifecycleEvent::Completed {
        status: RunCompletionStatus::Completed,
        summary: "done".into(),
    });
    assert!(
        matches!(
            completed.try_recv().expect("completion"),
            BusMessage::Completed { child, .. } if child.run_mode == ChildRunMode::Background
        ),
        "a promoted child's ending must read as a background ending"
    );
}

/// Without a promotion the ending stays a foreground ending, so the parent is
/// not notified twice for a child it already collected.
#[test]
fn test_unpromoted_child_stays_foreground() {
    let bus = Arc::new(AgentBus::new());
    let mut completed = bus.subscribe(global_completed_topic());
    let publisher = ChildStatusPublisher::new(Arc::clone(&bus), child(ChildRunMode::Foreground));
    publisher.handle(RunLifecycleEvent::Completed {
        status: RunCompletionStatus::Completed,
        summary: "done".into(),
    });
    assert!(matches!(
        completed.try_recv().expect("completion"),
        BusMessage::Completed { child, .. } if child.run_mode == ChildRunMode::Foreground
    ));
}
