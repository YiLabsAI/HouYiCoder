//! Publishes child lifecycle status on targeted and global bus topics.

use std::sync::Arc;

use houyicoder_api::agent_event::{EventHandler, RunCompletionStatus, RunLifecycleEvent};
use houyicoder_async::bus::MessageBus;
use houyicoder_protocol::frontend::completion::CompletionStatus;

use super::bus_types::{
    AgentBus, BusMessage, ChildDescriptor, completed_topic, global_completed_topic,
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
                    status: reported_ending(status),
                    summary,
                };
                self.bus
                    .publish(&completed_topic(&self.child.agent_id), message.clone());
                self.bus.publish(global_completed_topic(), message);
            }
        }
    }
}

/// The ending readers are told a child reached, from the engine's outcome.
fn reported_ending(status: RunCompletionStatus) -> CompletionStatus {
    match status {
        RunCompletionStatus::Completed | RunCompletionStatus::HandedOff => {
            CompletionStatus::Completed
        }
        RunCompletionStatus::Interrupted => CompletionStatus::Killed,
        RunCompletionStatus::Failed | RunCompletionStatus::VerificationFailed => {
            CompletionStatus::Failed
        }
        RunCompletionStatus::TurnLimitReached => CompletionStatus::TurnLimit,
    }
}

#[cfg(test)]
mod tests;
