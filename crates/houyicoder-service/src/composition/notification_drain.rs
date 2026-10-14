//! Completion notification injector. Subscribes to the global completion
//! topic; when a detached background child completes, enqueues a
//! lower-priority notification into the parent runner's mid-turn queue.
//! Sync children are skipped — their result returns as the tool result.
//!
//! Each completion carries its child descriptor, so the drain does not depend
//! on observing the earlier spawn announcement.

use std::sync::{Arc, Weak};

use houyicoder_async::CancellationToken;
use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, ChildRunMode, ChildStatus, global_completed_topic,
};
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

/// Lowercase label for a child's terminal status, mirrored from the fleet
/// projector so the footer strip and the notification text agree.
fn status_str(status: &ChildStatus) -> &'static str {
    match status {
        ChildStatus::Completed => "completed",
        ChildStatus::Killed => "killed",
        ChildStatus::Failed => "failed",
        ChildStatus::TurnLimit => "turn_limit",
        ChildStatus::BudgetExhausted => "budget",
    }
}

/// Format the notification text the parent reads as a mid-turn interjection:
/// subagent type + terminal status + the child's own summary, so the model
/// can act on the result without re-reading the child transcript.
fn notification_text(
    subagent_type: &str,
    child_id: &str,
    status: &ChildStatus,
    summary: &str,
) -> String {
    format!(
        "Subagent {subagent_type} ({short}) {status}: {summary}",
        short = &child_id[..child_id.len().min(8)],
        status = status_str(status),
    )
}

/// Spawn the injector. Subscribes to the global completion topic; on each
/// Completed from a background child, enqueues a notification. No-op when
/// the bus is absent. Holds the runner by a weak handle so the task never
/// keeps a released Runner alive; the cancel token ends the loop at shutdown.
/// Returns the task handle for the host lifecycle owner to reap.
pub fn spawn(
    bus: Option<Arc<AgentBus>>,
    runner: Weak<Runner>,
    runtime: Handle,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    let bus = bus?;
    let mut completed_rx = bus.subscribe(global_completed_topic());
    let handle = runtime.spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                recv = completed_rx.recv() => match recv {
                    Ok(BusMessage::Completed { child, status, summary }) => {
                        if child.run_mode == ChildRunMode::Background
                            && let Some(runner) = runner.upgrade()
                        {
                            let text = notification_text(
                                &child.agent_type,
                                &child.agent_id,
                                &status,
                                &summary,
                            );
                            runner.enqueue_notification(child.agent_id, text);
                        }
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
            }
        }
    });
    Some(handle)
}

#[cfg(test)]
mod tests;
