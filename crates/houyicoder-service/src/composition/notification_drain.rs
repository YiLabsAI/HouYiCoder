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
use houyicoder_core::agent::bound_child_summary;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, ChildRunMode, global_completed_topic,
};
use houyicoder_protocol::frontend::completion::CompletionStatus;
use houyicoder_protocol::frontend::event::FrontendEvent;
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::server::EventSequencer;

/// Format the notification text the parent reads as a mid-turn interjection:
/// subagent type + terminal status + the child's own summary, so the model can
/// act on the result without re-reading the child transcript, then the full
/// child session id on a line of its own.
///
/// The type line keeps the abbreviated id, which is what a reader scanning a
/// notification needs. The line below carries the whole one, because continuing
/// the child means passing its id back exactly, and a shortened id is not
/// something the model can reconstruct.
pub(crate) fn notification_text(
    subagent_type: &str,
    child_id: &str,
    status: &CompletionStatus,
    summary: &str,
) -> String {
    // The notice holds the same inline budget the delegation result does, so
    // a long background summary reaches the parent bounded on both paths.
    let summary = bound_child_summary(summary, child_id);
    format!(
        "Subagent {subagent_type} ({short}) {status}: {summary}\nresumable via {child_id}",
        short = &child_id[..child_id.len().min(8)],
        status = status.as_str(),
    )
}

/// Spawn the injector. On each Completed from a background child, enqueues a
/// notification and reports the queue depth to the frontend. Holds the runner
/// weakly; the cancel token ends the loop at shutdown.
pub fn spawn(
    bus: Option<Arc<AgentBus>>,
    runner: Weak<Runner>,
    event_sequencer: EventSequencer,
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
                            let count = runner.queued_notification_count();
                            event_sequencer
                                .enqueue_ephemeral(FrontendEvent::NotificationsPending { count });
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
