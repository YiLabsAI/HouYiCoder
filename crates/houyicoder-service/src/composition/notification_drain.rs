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
    runtime: tokio::runtime::Handle,
    cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
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
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
    Some(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_core::agent::ToolRegistry;
    use houyicoder_core::agent::multi_agent::bus_types::{ChildDescriptor, spawned_topic};
    use houyicoder_core::agent::runner_config::RunnerConfig;
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_session::SessionStore;

    fn child(id: &str, run_mode: ChildRunMode) -> ChildDescriptor {
        ChildDescriptor {
            agent_id: id.into(),
            agent_type: "explore".into(),
            run_mode,
        }
    }

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
    async fn test_background_completion_enqueues() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        spawn(
            Some(bus.clone()),
            Arc::downgrade(&runner),
            tokio::runtime::Handle::current(),
            CancellationToken::new(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                child: child("c1", ChildRunMode::Background),
            },
        );
        // No sleep: the global completion subscription exists at startup, so
        // publishing Completed immediately cannot lose it to a per-child
        // subscribe race. A yield lets the drain task process both messages.
        tokio::task::yield_now().await;
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                child: child("c1", ChildRunMode::Background),
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

    /// A foreground child is skipped: its result returns as the tool result, so no
    /// notification is enqueued even though its completion lands on the global
    /// topic alongside background children.
    #[tokio::test]
    async fn test_foreground_completion_skips_queue() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        spawn(
            Some(bus.clone()),
            Arc::downgrade(&runner),
            tokio::runtime::Handle::current(),
            CancellationToken::new(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                child: child("c2", ChildRunMode::Foreground),
            },
        );
        tokio::task::yield_now().await;
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                child: child("c2", ChildRunMode::Foreground),
                status: ChildStatus::Completed,
                summary: "done".into(),
            },
        );
        tokio::task::yield_now().await;
        assert!(
            runner.queued_notifications_snapshot().is_empty(),
            "foreground children do not enqueue a notification"
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
            Arc::downgrade(&runner),
            tokio::runtime::Handle::current(),
            CancellationToken::new(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                child: child("c3", ChildRunMode::Background),
            },
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                child: child("c3", ChildRunMode::Background),
            },
        );
        tokio::task::yield_now().await;
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                child: child("c3", ChildRunMode::Background),
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

    /// A failed background child also enqueues a completion notification.
    #[tokio::test]
    async fn test_failed_completion_enqueues() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        spawn(
            Some(bus.clone()),
            Arc::downgrade(&runner),
            tokio::runtime::Handle::current(),
            CancellationToken::new(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                child: child("c4", ChildRunMode::Background),
            },
        );
        tokio::task::yield_now().await;
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                child: child("c4", ChildRunMode::Background),
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

    /// Multiple background children completing near-simultaneously each enqueue a
    /// distinct notification (no cross-child dedup): the map is keyed by
    /// child id, so three children produce three notifications.
    #[tokio::test]
    async fn test_multi_child_concurrent_enqueues() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        spawn(
            Some(bus.clone()),
            Arc::downgrade(&runner),
            tokio::runtime::Handle::current(),
            CancellationToken::new(),
        );
        for id in ["c5", "c6", "c7"] {
            bus.publish(
                spawned_topic(),
                BusMessage::Spawned {
                    child: child(id, ChildRunMode::Background),
                },
            );
        }
        tokio::task::yield_now().await;
        for (id, summary) in [("c5", "found a"), ("c6", "found b"), ("c7", "found c")] {
            bus.publish(
                global_completed_topic(),
                BusMessage::Completed {
                    child: child(id, ChildRunMode::Background),
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
            Arc::downgrade(&runner),
            tokio::runtime::Handle::current(),
            CancellationToken::new(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                child: child("c8", ChildRunMode::Background),
            },
        );
        tokio::task::yield_now().await;
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                child: child("c8", ChildRunMode::Background),
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
            Arc::downgrade(&runner),
            tokio::runtime::Handle::current(),
            CancellationToken::new(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                child: child("c9", ChildRunMode::Background),
            },
        );
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                child: child("c9", ChildRunMode::Background),
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

    /// A self-contained completion still notifies when the earlier spawn
    /// announcement was not observed.
    #[tokio::test]
    async fn test_orphan_completion_notifies() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        spawn(
            Some(bus.clone()),
            Arc::downgrade(&runner),
            tokio::runtime::Handle::current(),
            CancellationToken::new(),
        );
        tokio::task::yield_now().await;
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                child: child("orphan", ChildRunMode::Background),
                status: ChildStatus::Completed,
                summary: "no spawn seen".into(),
            },
        );
        tokio::task::yield_now().await;
        let snap = runner.queued_notifications_snapshot();
        assert_eq!(
            snap.len(),
            1,
            "a background completion enqueues without a spawn announcement"
        );
        assert!(
            snap[0].contains("explore"),
            "carries the type from the Completed"
        );
        assert!(snap[0].contains("no spawn seen"), "carries the summary");
    }

    /// The cancel token ends the drain loop: a host shutdown cancels the
    /// token, the drain's select branch fires, and the task exits. This is
    /// the reaping contract the host lifecycle owner relies on so a session
    /// switch does not leave the old drain looping on the old bus.
    #[tokio::test]
    async fn test_cancel_ends_drain() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        let cancel = CancellationToken::new();
        let handle = spawn(
            Some(bus.clone()),
            Arc::downgrade(&runner),
            tokio::runtime::Handle::current(),
            cancel.clone(),
        )
        .expect("a bus present yields a handle");
        cancel.cancel();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
        assert!(outcome.is_ok(), "cancel ends the drain task");
        assert!(
            outcome.unwrap().is_ok(),
            "the drain task exited without panic"
        );
    }

    /// The drain holds the runner by a weak handle, so dropping the last
    /// strong reference releases the runner even while the drain task is still
    /// looping on the bus. Before this change the drain held an Arc and a
    /// released runner stayed alive until process exit.
    #[tokio::test]
    async fn test_drain_releases_runner() {
        let bus = Arc::new(AgentBus::new());
        let runner = bare_runner();
        let weak = Arc::downgrade(&runner);
        let cancel = CancellationToken::new();
        let _handle = spawn(
            Some(bus.clone()),
            Arc::downgrade(&runner),
            tokio::runtime::Handle::current(),
            cancel.clone(),
        );
        drop(runner);
        assert!(
            weak.upgrade().is_none(),
            "the drain holds only a weak handle so the runner is released"
        );
        cancel.cancel();
    }
}
