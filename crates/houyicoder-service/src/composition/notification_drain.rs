//! Completion notification injector. Subscribes to the global completion
//! topic; when a detached async child completes, enqueues a
//! lower-priority notification into the parent runner's mid-turn queue.
//! Sync children are skipped — their result returns as the tool result.
//!
//! The Completed message carries subagent_type and run_in_background so
//! the drain does not depend on the Spawned message, which can be lost to
//! broadcast lag — the prior silent-drop bug.

use std::sync::Arc;

use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, ChildStatus, global_completed_topic,
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

/// Spawn the injector. Subscribes to the global completion topic; on each
/// Completed from a background child, enqueues a notification. No-op when
/// the bus is absent.
pub fn spawn(bus: Option<Arc<AgentBus>>, runner: Arc<Runner>, runtime: tokio::runtime::Handle) {
    let Some(bus) = bus else {
        return;
    };
    let mut completed_rx = bus.subscribe(global_completed_topic());
    runtime.spawn(async move {
        loop {
            match completed_rx.recv().await {
                Ok(BusMessage::Completed {
                    agent_id,
                    status,
                    summary,
                    subagent_type,
                    run_in_background,
                }) => {
                    // Only async children get a completion notification —
                    // sync children's result returns as the tool result. The
                    // Completed message carries subagent_type and
                    // run_in_background, so the drain does not depend on the
                    // Spawned message, which can be lost to broadcast lag.
                    if run_in_background {
                        let text = notification_text(&subagent_type, &agent_id, &status, &summary);
                        runner.enqueue_notification(agent_id, text);
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_core::agent::ToolRegistry;
    use houyicoder_core::agent::multi_agent::bus_types::spawned_topic;
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
        // No sleep: the global completion subscription exists at startup, so
        // publishing Completed immediately cannot lose it to a per-child
        // subscribe race. A yield lets the drain task process both messages.
        tokio::task::yield_now().await;
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                agent_id: "c1".into(),
                status: ChildStatus::Completed,
                summary: "found auth".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        tokio::task::yield_now().await;
        let snap = runner.queued_notifications_snapshot();
        assert_eq!(snap.len(), 1, "one notification for one completion");
        assert!(snap[0].contains("explore"), "carries the subagent type");
        assert!(snap[0].contains("found auth"), "carries the summary");
    }

    /// A sync child is skipped: its result returns as the tool result, so no
    /// notification is enqueued even though its completion lands on the global
    /// topic alongside async children.
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
        tokio::task::yield_now().await;
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                agent_id: "c2".into(),
                status: ChildStatus::Completed,
                summary: "done".into(),
                subagent_type: "explore".into(),
                run_in_background: false,
            },
        );
        tokio::task::yield_now().await;
        assert!(
            runner.queued_notifications_snapshot().is_empty(),
            "sync children do not enqueue a notification"
        );
    }

    /// A duplicate Spawned for the same child cannot produce two
    /// notifications: the map holds the latest entry by key, and a single
    /// remove-on-complete yields one notification.
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
        tokio::task::yield_now().await;
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                agent_id: "c3".into(),
                status: ChildStatus::Completed,
                summary: "done".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        tokio::task::yield_now().await;
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
        tokio::task::yield_now().await;
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                agent_id: "c4".into(),
                status: ChildStatus::Failed,
                summary: "provider fatal".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        tokio::task::yield_now().await;
        let snap = runner.queued_notifications_snapshot();
        assert_eq!(snap.len(), 1, "a failed child still notifies the parent");
        assert!(snap[0].contains("failed"), "carries the failed status");
        assert!(snap[0].contains("provider fatal"), "carries the summary");
    }

    /// Multiple async children completing near-simultaneously each enqueue a
    /// distinct notification (no cross-child dedup): the map is keyed by
    /// child id, so three children produce three notifications.
    #[tokio::test]
    async fn test_multi_child_concurrent_enqueues() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        spawn(
            Some(bus.clone()),
            Arc::clone(&runner),
            tokio::runtime::Handle::current(),
        );
        for id in ["c5", "c6", "c7"] {
            bus.publish(
                spawned_topic(),
                BusMessage::Spawned {
                    agent_id: id.into(),
                    subagent_type: "explore".into(),
                    run_in_background: true,
                },
            );
        }
        tokio::task::yield_now().await;
        for (id, summary) in [("c5", "found a"), ("c6", "found b"), ("c7", "found c")] {
            bus.publish(
                global_completed_topic(),
                BusMessage::Completed {
                    agent_id: id.into(),
                    status: ChildStatus::Completed,
                    summary: summary.into(),
                    subagent_type: "explore".into(),
                    run_in_background: true,
                },
            );
        }
        tokio::task::yield_now().await;
        let snap = runner.queued_notifications_snapshot();
        assert_eq!(snap.len(), 3, "three children produce three notifications");
        let joined = snap.join(";");
        assert!(joined.contains("found a"), "c5 summary lands");
        assert!(joined.contains("found b"), "c6 summary lands");
        assert!(joined.contains("found c"), "c7 summary lands");
    }

    /// An empty summary does not crash or drop the notification — the parent
    /// still learns the child finished (type + status), just without a result
    /// line. Edge boundary for the summary field.
    #[tokio::test]
    async fn test_empty_summary_still_enqueues() {
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
                agent_id: "c8".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        tokio::task::yield_now().await;
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                agent_id: "c8".into(),
                status: ChildStatus::Completed,
                summary: String::new(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        tokio::task::yield_now().await;
        let snap = runner.queued_notifications_snapshot();
        assert_eq!(
            snap.len(),
            1,
            "an empty summary still enqueues a notification"
        );
        assert!(
            snap[0].contains("explore"),
            "carries the subagent type even with no summary"
        );
    }

    /// Race boundary: Spawned + Completed published back-to-back with no
    /// yield between them must still deliver the notification. This was the
    /// original subscribe-after-publish failure (a per-child subscribe landing
    /// after completion lost the one-shot terminal event). The global
    /// subscribe-before-publish guarantee + biased select make the loss
    /// structurally impossible.
    #[tokio::test]
    async fn test_back_to_back_deliver() {
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
                agent_id: "c9".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                agent_id: "c9".into(),
                status: ChildStatus::Completed,
                summary: "race".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let snap = runner.queued_notifications_snapshot();
        assert_eq!(
            snap.len(),
            1,
            "back-to-back spawn + completion must not lose the notification"
        );
        assert!(snap[0].contains("race"), "carries the summary");
    }

    /// A completion for a child whose Spawned was never published still
    /// enqueues a notification. The Completed message is self-contained,
    /// carrying subagent_type and run_in_background, so it does not depend
    /// on the Spawned message. Pins the broadcast-lag resilience.
    #[tokio::test]
    async fn test_orphan_completion_notifies() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        spawn(
            Some(bus.clone()),
            Arc::clone(&runner),
            tokio::runtime::Handle::current(),
        );
        tokio::task::yield_now().await;
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                agent_id: "orphan".into(),
                status: ChildStatus::Completed,
                summary: "no spawn seen".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        tokio::task::yield_now().await;
        let snap = runner.queued_notifications_snapshot();
        assert_eq!(
            snap.len(),
            1,
            "a Completed with run_in_background=true enqueues without a Spawned"
        );
        assert!(
            snap[0].contains("explore"),
            "carries the type from the Completed"
        );
        assert!(snap[0].contains("no spawn seen"), "carries the summary");
    }
}
