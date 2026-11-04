//! Publishes child lifecycle status on targeted and global bus topics.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use houyicoder_api::agent_event::{
    EventHandler, LivenessPhase, RunCompletionStatus, RunLifecycleEvent,
};
use houyicoder_async::bus::MessageBus;
use houyicoder_context::SessionId;
use houyicoder_protocol::frontend::completion::CompletionStatus;

use super::bus_types::{
    AgentBus, BusMessage, ChildDescriptor, ChildRunMode, completed_topic, global_completed_topic,
    global_progress_topic, progress_topic,
};
use super::ending::EndingClaim;
use super::spawn::CutReason;
use super::spawn::PartialTranscript;

/// The liveness state a watchdog reads: when the last pulse arrived and what
/// phase the run was in. A stalled child (no pulse within the idle window) is
/// cut with the phase naming where it stopped.
pub struct ChildLiveness {
    last_pulse_ms: AtomicU64,
    phase: Mutex<LivenessPhase>,
}

impl Default for ChildLiveness {
    fn default() -> Self {
        Self::new()
    }
}

impl ChildLiveness {
    pub fn new() -> Self {
        Self {
            last_pulse_ms: AtomicU64::new(now_ms()),
            phase: Mutex::new(LivenessPhase::Streaming),
        }
    }

    pub fn record(&self, phase: LivenessPhase) {
        self.last_pulse_ms.store(now_ms(), Ordering::Release);
        if let Ok(mut p) = self.phase.lock() {
            *p = phase;
        }
    }

    pub fn last_pulse_ms(&self) -> u64 {
        self.last_pulse_ms.load(Ordering::Acquire)
    }

    pub fn phase(&self) -> LivenessPhase {
        self.phase
            .lock()
            .map(|p| *p)
            .unwrap_or(LivenessPhase::Streaming)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Publishes lifecycle status for one child.
pub struct ChildStatusPublisher {
    bus: Arc<AgentBus>,
    child: ChildDescriptor,
    ending: Arc<EndingClaim>,
    cut_reason: Option<Arc<Mutex<Option<CutReason>>>>,
    partial: Option<Arc<dyn PartialTranscript>>,
    liveness: Option<Arc<ChildLiveness>>,
}

impl ChildStatusPublisher {
    /// Bind a publisher to one child descriptor.
    pub fn new(bus: Arc<AgentBus>, child: ChildDescriptor) -> Self {
        let ending = Arc::new(EndingClaim::new(child.run_mode));
        Self {
            bus,
            child,
            ending,
            cut_reason: None,
            partial: None,
            liveness: None,
        }
    }

    pub fn with_liveness(mut self, liveness: Arc<ChildLiveness>) -> Self {
        self.liveness = Some(liveness);
        self
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

    /// The claim the runtime shares with the wait on this child.
    pub fn ending(&self) -> Arc<EndingClaim> {
        Arc::clone(&self.ending)
    }

    /// Publish through a claim another reporter already holds, so both read
    /// and set the same decision.
    pub fn with_ending(mut self, ending: Arc<EndingClaim>) -> Self {
        self.ending = ending;
        self
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
            RunLifecycleEvent::Pulse { phase } => {
                if let Some(liveness) = self.liveness.as_ref() {
                    liveness.record(phase);
                }
            }
            RunLifecycleEvent::Completed { status, summary } => {
                // The ending carries the mode the child has now, not the one it
                // started in: a child the reader handed to the background must
                // still notify the parent, and one whose wait gave up must not
                // report a foreground ending that would be dropped.
                let mut child = self.child.clone();
                if !self.ending.claim_caller() {
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
