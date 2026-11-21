//! Tests for this module.

use super::{QueuedInput, Runner};
use crate::agent::ToolRegistry;
use crate::agent::runner_config::RunnerConfig;
use crate::provider::test_support::FakeProvider;
use houyicoder_api::agent_event::{AgentEventHandlers, NotificationQueueEvent};
use houyicoder_context::{NotificationTopic, SessionId};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;
use std::sync::{Arc, Mutex};

/// One reported queue level: the transition stamp and the depth it left.
type ReportedLevel = (u64, u32);

/// The levels a runner reported, shared with the handler that records them.
type ReportedLevels = Arc<Mutex<Vec<ReportedLevel>>>;

fn bare_runner() -> Runner {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Runner::new(
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
    )
}

/// Removing one duplicate targets its identity and leaves the other copy.
#[test]
fn test_remove_exact_id() {
    let r = bare_runner();
    let first = QueuedInput::new("same");
    let second = QueuedInput::new("same");
    r.enqueue_input(first.clone());
    r.enqueue_input(second.clone());
    r.remove_input(second.id);
    assert_eq!(r.queued_input_snapshot(), vec![first.text]);
}

/// Collect the stamps and counts the runner reports on its queue handler.
fn counting_runner() -> (Runner, ReportedLevels) {
    let mut r = bare_runner();
    let seen: ReportedLevels = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut events = AgentEventHandlers::default();
    events.set_notification_queue(Arc::new(move |event: NotificationQueueEvent| {
        sink.lock()
            .expect("levels lock")
            .push((event.generation, event.count));
    }));
    r.set_event_handlers(events);
    (r, seen)
}

/// The depths a runner reported, in the order it reported them.
fn reported_depths(seen: &ReportedLevels) -> Vec<u32> {
    seen.lock()
        .expect("levels lock")
        .iter()
        .map(|(_, count)| *count)
        .collect()
}

/// A finished background command must report a non-zero level, because that
/// frame is the only thing that wakes an idle client. A producer that enqueues
/// and says nothing leaves the client at zero, so it never wakes.
#[test]
fn test_enqueue_notification_reports_level() {
    let (r, seen) = counting_runner();
    r.enqueue_notification(
        "bash-1".into(),
        NotificationTopic::Task,
        "done".into(),
        None,
    );
    assert_eq!(reported_depths(&seen), vec![1]);
}

/// Consuming a notification must report the level that is left, not silence.
/// A client that kept the old positive count wakes an empty turn once the run
/// finishes, which burns one auto-wake turn for nothing.
#[tokio::test]
async fn test_drain_reports_remaining_level() {
    let (r, seen) = counting_runner();
    r.enqueue_notification(
        "bash-1".into(),
        NotificationTopic::Task,
        "done".into(),
        None,
    );
    r.drain_turn_boundary(SessionId::new())
        .await
        .expect("drain");
    assert_eq!(reported_depths(&seen), vec![1, 0]);
}

/// The level a reader attaches with carries a stamp taken under the queue
/// lock, so it orders against the producers' reports. A drain that lands
/// after that reading must carry a newer stamp: the older reading cannot
/// resurrect a count the queue has already left, which would wake the client
/// for an empty queue.
#[tokio::test]
async fn test_attach_report_is_ordered() {
    let (r, seen) = counting_runner();
    r.enqueue_notification(
        "bash-1".into(),
        NotificationTopic::Task,
        "done".into(),
        None,
    );
    let attach = r.stamp_queued_notification_level();
    assert_eq!(
        attach.map(|(_, count)| count),
        Some(1),
        "the stamp carries the queue depth"
    );
    let attach_generation = attach.expect("a non-empty queue reports").0;
    r.drain_turn_boundary(SessionId::new())
        .await
        .expect("drain");
    let stamps: Vec<u64> = seen
        .lock()
        .expect("levels lock")
        .iter()
        .map(|(generation, _)| *generation)
        .collect();
    assert!(
        stamps
            .last()
            .is_some_and(|newest| *newest > attach_generation),
        "the report after the attach reading is newer: {stamps:?} vs {attach_generation}"
    );
    assert_eq!(
        seen.lock().expect("levels lock").last().map(|(_, c)| *c),
        Some(0),
        "the drain's report is the newest, so the client ends at zero"
    );
}
