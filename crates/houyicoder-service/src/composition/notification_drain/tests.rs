//! Tests for this module.

use super::*;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::multi_agent::bus_types::{ChildDescriptor, spawned_topic};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::frontend::completion::CompletionStatus;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::task::yield_now;
use tokio::time::timeout;

fn child(id: &str, run_mode: ChildRunMode) -> ChildDescriptor {
    ChildDescriptor {
        agent_id: id.into(),
        agent_type: "explore".into(),
        description: String::new(),
        run_mode,
    }
}

fn bare_runner() -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(Runner::new(
        store,
        Arc::new(FakeProvider::text("ok")),
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
        EventSequencer::new(),
        Handle::current(),
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
    yield_now().await;
    bus.publish(
        global_completed_topic(),
        BusMessage::Completed {
            child: child("c1", ChildRunMode::Background),
            status: CompletionStatus::Completed,
            summary: "found auth".into(),
        },
    );
    yield_now().await;
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
        EventSequencer::new(),
        Handle::current(),
        CancellationToken::new(),
    );
    bus.publish(
        spawned_topic(),
        BusMessage::Spawned {
            child: child("c2", ChildRunMode::Foreground),
        },
    );
    yield_now().await;
    bus.publish(
        global_completed_topic(),
        BusMessage::Completed {
            child: child("c2", ChildRunMode::Foreground),
            status: CompletionStatus::Completed,
            summary: "done".into(),
        },
    );
    yield_now().await;
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
        EventSequencer::new(),
        Handle::current(),
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
    yield_now().await;
    bus.publish(
        global_completed_topic(),
        BusMessage::Completed {
            child: child("c3", ChildRunMode::Background),
            status: CompletionStatus::Completed,
            summary: "done".into(),
        },
    );
    yield_now().await;
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
        EventSequencer::new(),
        Handle::current(),
        CancellationToken::new(),
    );
    bus.publish(
        spawned_topic(),
        BusMessage::Spawned {
            child: child("c4", ChildRunMode::Background),
        },
    );
    yield_now().await;
    bus.publish(
        global_completed_topic(),
        BusMessage::Completed {
            child: child("c4", ChildRunMode::Background),
            status: CompletionStatus::Failed,
            summary: "provider fatal".into(),
        },
    );
    yield_now().await;
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
        EventSequencer::new(),
        Handle::current(),
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
    yield_now().await;
    for (id, summary) in [("c5", "found a"), ("c6", "found b"), ("c7", "found c")] {
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                child: child(id, ChildRunMode::Background),
                status: CompletionStatus::Completed,
                summary: summary.into(),
            },
        );
    }
    yield_now().await;
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
        EventSequencer::new(),
        Handle::current(),
        CancellationToken::new(),
    );
    bus.publish(
        spawned_topic(),
        BusMessage::Spawned {
            child: child("c8", ChildRunMode::Background),
        },
    );
    yield_now().await;
    bus.publish(
        global_completed_topic(),
        BusMessage::Completed {
            child: child("c8", ChildRunMode::Background),
            status: CompletionStatus::Completed,
            summary: String::new(),
        },
    );
    yield_now().await;
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
        EventSequencer::new(),
        Handle::current(),
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
            status: CompletionStatus::Completed,
            summary: "race".into(),
        },
    );
    yield_now().await;
    yield_now().await;
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
        EventSequencer::new(),
        Handle::current(),
        CancellationToken::new(),
    );
    yield_now().await;
    bus.publish(
        global_completed_topic(),
        BusMessage::Completed {
            child: child("orphan", ChildRunMode::Background),
            status: CompletionStatus::Completed,
            summary: "no spawn seen".into(),
        },
    );
    yield_now().await;
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
        EventSequencer::new(),
        Handle::current(),
        cancel.clone(),
    )
    .expect("a bus present yields a handle");
    cancel.cancel();
    let outcome = timeout(Duration::from_secs(1), handle).await;
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
        EventSequencer::new(),
        Handle::current(),
        cancel.clone(),
    );
    drop(runner);
    assert!(
        weak.upgrade().is_none(),
        "the drain holds only a weak handle so the runner is released"
    );
    cancel.cancel();
}

/// Each enqueued notification also reports the queue depth to the frontend,
/// so an idle client learns the session holds something to wake for. Two
/// completions produce two reports, each naming the depth after its own
/// enqueue.
#[tokio::test]
async fn test_enqueue_reports_pending_count() {
    let bus = Arc::new(AgentBus::new());
    let runner = bare_runner();
    let sequencer = EventSequencer::new();
    spawn(
        Some(bus.clone()),
        Arc::downgrade(&runner),
        sequencer.clone(),
        Handle::current(),
        CancellationToken::new(),
    );
    for (id, summary) in [("d1", "found a"), ("d2", "found b")] {
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                child: child(id, ChildRunMode::Background),
                status: CompletionStatus::Completed,
                summary: summary.into(),
            },
        );
        yield_now().await;
    }
    yield_now().await;
    let snap = runner.queued_notifications_snapshot();
    assert_eq!(snap.len(), 2, "both completions enqueued");
    assert_eq!(
        runner.queued_notification_count(),
        2,
        "the count matches the queue depth"
    );
}

/// The notification names the child twice: a shortened id on the type line,
/// where a reader only has to tell one child from another, and the whole id
/// below, because continuing the child means passing that id back exactly.
#[test]
fn test_notification_names_full_child() {
    let id = "01a2b3c4d5e6f708192a3b4c5d";
    let text = notification_text("explore", id, &CompletionStatus::Completed, "found auth");
    assert!(
        text.contains("(01a2b3c4)"),
        "the type line keeps the short form: {text}"
    );
    assert!(
        text.contains(&format!("resumable via {id}")),
        "the full id is there to continue the child: {text}"
    );
}

/// A background completion notice holds the same inline budget as a
/// delegation result: a long summary keeps its head and tail with a spill
/// pointer naming the child, so a long child report cannot crowd the
/// parent's context through the notification path either.
#[test]
fn test_notice_bounds_summary() {
    let long = "x".repeat(9_000);
    let text = notification_text(
        "explore",
        "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        &CompletionStatus::Completed,
        &long,
    );
    assert!(
        text.chars().count() < long.chars().count(),
        "the notice is smaller than the raw summary"
    );
    assert!(
        text.contains("task_output with task_id=\"01ARZ3NDEKTSV4RRFFQ69G5FAV\""),
        "the spill pointer names the child"
    );
}

/// A short summary passes through the notice untouched: the budget only
/// bounds the overlong, so ordinary endings read exactly what the child said.
#[test]
fn test_notice_keeps_short_summary() {
    let text = notification_text(
        "explore",
        "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        &CompletionStatus::Completed,
        "done",
    );
    assert!(text.contains("done"), "{text}");
    assert!(!text.contains("truncated"), "{text}");
}
