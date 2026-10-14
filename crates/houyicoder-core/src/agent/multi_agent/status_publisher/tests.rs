//! Tests for this module.

use super::super::bus_types::ChildRunMode;
use super::*;

fn child(run_mode: ChildRunMode) -> ChildDescriptor {
    ChildDescriptor {
        agent_id: "child-1".into(),
        agent_type: "explore".into(),
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
            BusMessage::Completed { child, status: ChildStatus::Completed, .. }
                if child.run_mode == run_mode
        ));
    }
}

#[test]
fn test_completion_maps_child_status() {
    assert_eq!(
        child_status(RunCompletionStatus::HandedOff),
        ChildStatus::Completed
    );
    assert_eq!(
        child_status(RunCompletionStatus::Interrupted),
        ChildStatus::Killed
    );
    assert_eq!(
        child_status(RunCompletionStatus::VerificationFailed),
        ChildStatus::Failed
    );
    assert_eq!(
        child_status(RunCompletionStatus::TurnLimitReached),
        ChildStatus::TurnLimit
    );
}
