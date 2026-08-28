//! Completion notification injector: subscribes to the multi-agent bus and,
//! when a detached (async) child completes, enqueues a lower-priority
//! notification into the parent runner's mid-turn queue so the parent model
//! learns the child finished on its next turn boundary. Sync children are
//! skipped (their result returns as the tool result); a duplicate signal for
//! the same child dedups to one notification via a check-and-set.

use std::collections::HashSet;
use std::sync::Arc;

use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, ChildStatus, completed_topic, spawned_topic,
};

/// Lowercase label for a child's terminal status, mirrored from the fleet
/// projector so the footer pill and the notification text agree.
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

/// Spawn the injector. Subscribes to the spawned topic; per detached child,
/// subscribes to that child's completed topic. On Completed, a check-and-set
/// on the shared notified set drops a second signal for the same child, and
/// the notification is enqueued into the parent's lower-priority mid-turn
/// queue. No-op when the bus is absent.
pub fn spawn(bus: Option<Arc<AgentBus>>, runner: Arc<Runner>, runtime: tokio::runtime::Handle) {
    let Some(bus) = bus else {
        return;
    };
    let mut spawned_rx = bus.subscribe(spawned_topic());
    let notified: Arc<std::sync::Mutex<HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(HashSet::new()));
    let inner_runtime = runtime.clone();
    runtime.spawn(async move {
        loop {
            // On Lagged (a burst overflowed the broadcast buffer) skip the
            // missed announcement + keep draining; on Closed (all bus
            // senders gone) exit so the task does not tight-loop on a dead
            // receiver. A child whose Spawned was lost to lag is later
            // covered by its SubagentReturn boundary in the parent log.
            match spawned_rx.recv().await {
                Ok(BusMessage::Spawned {
                    agent_id,
                    subagent_type,
                    run_in_background,
                }) => {
                    // Sync children return their result as the tool result; a
                    // notification here would double-tell the parent.
                    if !run_in_background {
                        continue;
                    }
                    let completed_rx = bus.subscribe(&completed_topic(&agent_id));
                    inner_runtime.spawn(drain_completion(
                        agent_id,
                        subagent_type,
                        completed_rx,
                        Arc::clone(&notified),
                        Arc::clone(&runner),
                    ));
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Drain one child's completed topic. On the first Completed, check-and-set
/// the shared notified set + enqueue if this is the first signal for the
/// child, then return (the child is terminal).
async fn drain_completion(
    child_id: String,
    subagent_type: String,
    mut completed_rx: tokio::sync::broadcast::Receiver<BusMessage>,
    notified: Arc<std::sync::Mutex<HashSet<String>>>,
    runner: Arc<Runner>,
) {
    while let Ok(msg) = completed_rx.recv().await {
        if let BusMessage::Completed {
            status, summary, ..
        } = msg
        {
            // Check-and-set: a duplicate Spawned race or a double-publish
            // lands a second signal; the first insert wins, the second drops.
            let fresh = notified
                .lock()
                .expect("notified set lock")
                .insert(child_id.clone());
            if fresh {
                runner.enqueue_notification(notification_text(
                    &subagent_type,
                    &child_id,
                    &status,
                    &summary,
                ));
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_core::agent::ToolRegistry;
    use houyicoder_core::agent::runner_config::RunnerConfig;
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_session::SessionStore;

    fn bare_runner() -> Arc<Runner> {
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        Arc::new(Runner::new(
            store,
            Arc::new(houyicoder_provider::FakeProvider::text("ok")),
            ToolRegistry::new(),
            RunnerConfig {
                model: "test".into(),
                instructions: "test".into(),
                max_turns: 5,
                max_output_tokens: 8_000,
                ..RunnerConfig::default()
            },
        ))
    }

    /// A detached child's completion enqueues one notification carrying the
    /// subagent type + summary into the parent's lower-priority queue.
    #[tokio::test]
    async fn test_async_completion_enqueues() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        spawn(
            Some(bus.clone()),
            Arc::clone(&runner),
            tokio::runtime::Handle::current(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                agent_id: "c1".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        // Let the dispatch task subscribe before publishing completed.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bus.publish(
            &completed_topic("c1"),
            BusMessage::Completed {
                agent_id: "c1".into(),
                status: ChildStatus::Completed,
                summary: "found auth".into(),
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let snap = runner.queued_notifications_snapshot();
        assert_eq!(snap.len(), 1, "one notification for one completion");
        assert!(snap[0].contains("explore"), "carries the subagent type");
        assert!(snap[0].contains("found auth"), "carries the summary");
    }

    /// A sync child is skipped: its result returns as the tool result, so no
    /// notification is enqueued.
    #[tokio::test]
    async fn test_sync_completion_skipped() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        spawn(
            Some(bus.clone()),
            Arc::clone(&runner),
            tokio::runtime::Handle::current(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                agent_id: "c2".into(),
                subagent_type: "explore".into(),
                run_in_background: false,
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bus.publish(
            &completed_topic("c2"),
            BusMessage::Completed {
                agent_id: "c2".into(),
                status: ChildStatus::Completed,
                summary: "done".into(),
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            runner.queued_notifications_snapshot().is_empty(),
            "sync children do not enqueue a notification"
        );
    }

    /// A duplicate signal for the same child (double Spawned race) dedups to
    /// one notification via the check-and-set.
    #[tokio::test]
    async fn test_duplicate_signal_dedups() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        spawn(
            Some(bus.clone()),
            Arc::clone(&runner),
            tokio::runtime::Handle::current(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                agent_id: "c3".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                agent_id: "c3".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bus.publish(
            &completed_topic("c3"),
            BusMessage::Completed {
                agent_id: "c3".into(),
                status: ChildStatus::Completed,
                summary: "done".into(),
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            runner.queued_notifications_snapshot().len(),
            1,
            "a duplicate signal for the same child dedups to one"
        );
    }

    /// A failed child also enqueues a notification (the parent learns of
    /// failure, not only success). Pins that the drain matches every terminal
    /// status, so a later narrowing to Completed-only would regress red.
    #[tokio::test]
    async fn test_failed_completion_enqueues() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        spawn(
            Some(bus.clone()),
            Arc::clone(&runner),
            tokio::runtime::Handle::current(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                agent_id: "c4".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bus.publish(
            &completed_topic("c4"),
            BusMessage::Completed {
                agent_id: "c4".into(),
                status: ChildStatus::Failed,
                summary: "provider fatal".into(),
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let snap = runner.queued_notifications_snapshot();
        assert_eq!(snap.len(), 1, "a failed child still notifies the parent");
        assert!(snap[0].contains("failed"), "carries the failed status");
        assert!(snap[0].contains("provider fatal"), "carries the summary");
    }
}
