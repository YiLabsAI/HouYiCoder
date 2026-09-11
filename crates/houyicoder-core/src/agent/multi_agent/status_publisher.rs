//! Publishes child lifecycle status on targeted and global bus topics.

use std::sync::Arc;

use houyicoder_api::agent_event::{EventHandler, RunCompletionStatus, RunLifecycleEvent};
use houyicoder_async::bus::MessageBus;

use super::bus_types::{
    AgentBus, BusMessage, ChildDescriptor, ChildStatus, completed_topic, global_completed_topic,
    global_progress_topic, progress_topic,
};

/// Publishes lifecycle status for one child.
pub struct ChildStatusPublisher {
    bus: Arc<AgentBus>,
    child: ChildDescriptor,
}

impl ChildStatusPublisher {
    /// Bind a publisher to one child descriptor.
    pub fn new(bus: Arc<AgentBus>, child: ChildDescriptor) -> Self {
        Self { bus, child }
    }
}

impl EventHandler<RunLifecycleEvent> for ChildStatusPublisher {
    fn handle(&self, event: RunLifecycleEvent) {
        match event {
            RunLifecycleEvent::TurnCompleted {
                turn,
                cumulative_tokens,
                tool_uses,
                last_activity,
            } => {
                let message = BusMessage::Progress {
                    child: self.child.clone(),
                    turn,
                    tokens: cumulative_tokens,
                    tool_uses,
                    last_activity,
                };
                self.bus
                    .publish(&progress_topic(&self.child.agent_id), message.clone());
                self.bus.publish(global_progress_topic(), message);
            }
            RunLifecycleEvent::Completed { status, summary } => {
                let message = BusMessage::Completed {
                    child: self.child.clone(),
                    status: child_status(status),
                    summary,
                };
                self.bus
                    .publish(&completed_topic(&self.child.agent_id), message.clone());
                self.bus.publish(global_completed_topic(), message);
            }
        }
    }
}

fn child_status(status: RunCompletionStatus) -> ChildStatus {
    match status {
        RunCompletionStatus::Completed | RunCompletionStatus::HandedOff => ChildStatus::Completed,
        RunCompletionStatus::Interrupted => ChildStatus::Killed,
        RunCompletionStatus::Failed | RunCompletionStatus::VerificationFailed => {
            ChildStatus::Failed
        }
        RunCompletionStatus::TurnLimitReached => ChildStatus::TurnLimit,
    }
}

#[cfg(test)]
mod tests {
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
}
