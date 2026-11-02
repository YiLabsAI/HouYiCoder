//! Publishes child lifecycle status on targeted and global bus topics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use houyicoder_api::agent_event::{EventHandler, RunCompletionStatus, RunLifecycleEvent};
use houyicoder_async::bus::MessageBus;
use houyicoder_protocol::frontend::completion::CompletionStatus;

use super::bus_types::{
    AgentBus, BusMessage, ChildDescriptor, ChildRunMode, completed_topic, global_completed_topic,
    global_progress_topic, progress_topic,
};

/// Publishes lifecycle status for one child.
pub struct ChildStatusPublisher {
    bus: Arc<AgentBus>,
    child: ChildDescriptor,
    /// Whether this child's ending must reach the parent as a notification.
    ///
    /// A spawn decides the mode, but a foreground child can be handed to the
    /// background while it runs. The mode therefore lives behind a shared flag
    /// the runtime can flip, and the ending reads it at publish time rather
    /// than at construction: a child promoted mid-flight still tells the
    /// parent it finished, and one that was never promoted stays silent.
    backgrounded: Arc<AtomicBool>,
}

impl ChildStatusPublisher {
    /// Bind a publisher to one child descriptor.
    pub fn new(bus: Arc<AgentBus>, child: ChildDescriptor) -> Self {
        let backgrounded = Arc::new(AtomicBool::new(matches!(
            child.run_mode,
            ChildRunMode::Background
        )));
        Self {
            bus,
            child,
            backgrounded,
        }
    }

    /// The flag the runtime flips to hand this child to the background.
    pub fn backgrounded(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.backgrounded)
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
                // The ending carries the mode the child has now, not the one it
                // started in: a foreground child the reader handed to the
                // background must still notify the parent.
                let mut child = self.child.clone();
                if self.backgrounded.load(Ordering::Acquire) {
                    child.run_mode = ChildRunMode::Background;
                }
                let message = BusMessage::Completed {
                    child,
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
