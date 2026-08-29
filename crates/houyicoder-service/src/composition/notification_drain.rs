//! Completion notification injector. Subscribes to the global bus topics;
//! when a detached (async) child completes, enqueues a lower-priority
//! notification into the parent runner's mid-turn queue. Sync children are
//! skipped (their result returns as the tool result).
//!
//! Race-free: both subscriptions exist at startup, before any child spawns,
//! so a per-child subscribe landing after completion cannot miss the
//! one-shot terminal event. A biased select drains Spawned before Completed.

use std::collections::HashMap;
use std::sync::Arc;

use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, ChildStatus, global_completed_topic, spawned_topic,
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

/// Spawn the injector. On Spawned with run_in_background, record the child in
/// a local map; on Completed, if recorded, enqueue the notification and
/// remove the entry (a duplicate completion dedups to one). No-op when the
/// bus is absent.
pub fn spawn(bus: Option<Arc<AgentBus>>, runner: Arc<Runner>, runtime: tokio::runtime::Handle) {
    let Some(bus) = bus else {
        return;
    };
    let mut spawned_rx = bus.subscribe(spawned_topic());
    let mut completed_rx = bus.subscribe(global_completed_topic());
    runtime.spawn(async move {
        // agent_id -> subagent_type, async children only. Remove-on-complete
        // keeps the map bounded to in-flight async children.
        let mut pending: HashMap<String, String> = HashMap::new();
        loop {
            // Biased: drain Spawned before Completed when both ready. A
            // child's Spawned is always announced before its Completed, so
            // the map entry exists by the time the matching Completed lands.
            tokio::select! {
                biased;
                msg = spawned_rx.recv() => match msg {
                    Ok(BusMessage::Spawned {
                        agent_id,
                        subagent_type,
                        run_in_background,
                    }) => {
                        if run_in_background {
                            pending.insert(agent_id, subagent_type);
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                msg = completed_rx.recv() => match msg {
                    Ok(BusMessage::Completed {
                        agent_id,
                        status,
                        summary,
                    }) => {
                        // A missing entry is a sync child or a Spawned lost
                        // to broadcast lag. The lag case: SubagentReturn is
                        // in the parent log (replay/audit honest) but does
                        // not project into model input — the model misses
                        // this child's result this turn. Bounded by the cap.
                        if let Some(subagent_type) = pending.remove(&agent_id) {
                            let text =
                                notification_text(&subagent_type, &agent_id, &status, &summary);
                            runner.enqueue_notification(agent_id, text);
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
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

    /// A completion for a child whose Spawned was never published (or was lost
    /// to broadcast lag) does not enqueue a notification and does not panic.
    /// The parent still learns of the result via the SubagentReturn boundary
    /// in its own log. Pins the missing-entry path as a clean skip.
    #[tokio::test]
    async fn test_completion_without_spawn_skips() {
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
            },
        );
        tokio::task::yield_now().await;
        assert!(
            runner.queued_notifications_snapshot().is_empty(),
            "an unknown child completion is skipped, not notified"
        );
    }
}
