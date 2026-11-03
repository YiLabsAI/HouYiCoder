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
use houyicoder_context::NotificationTopic;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, ChildRunMode, global_completed_topic,
};
use houyicoder_protocol::frontend::completion::CompletionStatus;
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

/// Format the notification text the parent reads as a mid-turn interjection:
/// subagent type + terminal status + the child's own summary, so the model can
/// act on the result without re-reading the child transcript, then the full
/// child session id on a line of its own.
///
/// The type line keeps the abbreviated id, which is what a reader scanning a
/// notification needs. The line below carries the whole one, because continuing
/// the child means passing its id back exactly, and a shortened id is not
/// something the model can reconstruct.
fn notification_text(
    subagent_type: &str,
    child_id: &str,
    status: &CompletionStatus,
    summary: &str,
) -> String {
    format!(
        "Subagent {subagent_type} ({short}) {status}: {summary}\nresumable via {child_id}",
        short = &child_id[..child_id.len().min(8)],
        status = status.as_str(),
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
                            runner.enqueue_notification(
                                child.agent_id,
                                NotificationTopic::ChildCompletion,
                                text,
                                None,
                            );
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
