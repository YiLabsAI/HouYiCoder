//! Publishes child lifecycle status on targeted and global bus topics.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use houyicoder_api::agent_event::{EventHandler, RunCompletionStatus, RunLifecycleEvent};
use houyicoder_async::bus::MessageBus;
use houyicoder_context::SessionId;
use houyicoder_protocol::frontend::completion::CompletionStatus;

use super::bus_types::{
    AgentBus, BusMessage, ChildDescriptor, ChildRunMode, completed_topic, global_completed_topic,
    global_progress_topic, progress_topic,
};
use super::spawn::CutReason;
use super::spawn::PartialTranscript;

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
    /// The reason an outside stop ended this child, when one did.
    ///
    /// The ending is published from inside the child's run, after the stop was
    /// requested and before the spawn code that requested it gets the run back.
    /// A stop that wants to name its cause therefore records it in this slot
    /// first, and the ending reads it here.
    cut_reason: Option<Arc<Mutex<Option<CutReason>>>>,
    /// Reads the child's last assistant text, so an ending cut short reports
    /// what the child had produced rather than the stop reason alone.
    partial: Option<Arc<dyn PartialTranscript>>,
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
            cut_reason: None,
            partial: None,
        }
    }

    /// Install the cut reason this publisher shares with the child handle and,
    /// when the caller has one, the transcript that supplies the child's
    /// partial output. Without a transcript a cut ending still reports itself
    /// as one, falling back to the ending's own summary.
    pub fn with_cut_report(
        mut self,
        cut_reason: Arc<Mutex<Option<CutReason>>>,
        partial: Option<Arc<dyn PartialTranscript>>,
    ) -> Self {
        self.cut_reason = Some(cut_reason);
        self.partial = partial;
        self
    }

    /// The flag the runtime flips to hand this child to the background.
    pub fn backgrounded(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.backgrounded)
    }

    /// The cut this child was stopped by, when one was recorded.
    fn recorded_cut(&self) -> Option<CutReason> {
        self.cut_reason.as_ref().and_then(|slot| recorded_cut(slot))
    }

    /// What the child had said when it stopped: its last assistant text, or
    /// the ending's own summary when no transcript reader is installed, the
    /// agent id names no session, or the child produced no text.
    fn partial_output(&self, summary: &str) -> String {
        let Some(reader) = self.partial.as_ref() else {
            return summary.to_string();
        };
        let Some(session) = SessionId::from_display_string(&self.child.agent_id) else {
            return summary.to_string();
        };
        reader
            .last_text(session)
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| summary.to_string())
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
                let cut = self.recorded_cut();
                let (status, summary) = match &cut {
                    Some(reason @ CutReason::TimedOut { .. }) => {
                        let partial = self.partial_output(&summary);
                        (
                            CompletionStatus::TimedOut,
                            timed_out_summary(reason, &self.child.agent_id, &partial),
                        )
                    }
                    None => (reported_ending(status), summary),
                };
                let message = BusMessage::Completed {
                    child,
                    status,
                    summary,
                };
                self.bus
                    .publish(&completed_topic(&self.child.agent_id), message.clone());
                self.bus.publish(global_completed_topic(), message);
            }
        }
    }
}

/// The cut recorded for a child, when one was.
///
/// A poisoned slot reports none: the reason is advisory, and a panic in a
/// writer must not turn every later read of the ending into a panic of its own.
pub fn recorded_cut(slot: &Mutex<Option<CutReason>>) -> Option<CutReason> {
    slot.lock().ok().and_then(|cut| cut.clone())
}

/// The summary a child reports when an outside stop cut its run short: what
/// stopped it, the id a later delegation can continue it by, and what it had
/// produced by then.
pub fn timed_out_summary(reason: &CutReason, child_id: &str, partial: &str) -> String {
    match reason {
        CutReason::TimedOut { secs, .. } => {
            format!("[timed out after {secs}s, resumable via {child_id}]\n{partial}")
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
