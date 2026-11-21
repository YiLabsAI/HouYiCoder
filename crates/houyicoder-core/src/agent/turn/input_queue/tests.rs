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

/// Collect the levels the runner reports on its notification-queue handler.
fn counting_runner() -> (Runner, Arc<Mutex<Vec<u32>>>) {
    let mut r = bare_runner();
    let seen: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut events = AgentEventHandlers::default();
    events.set_notification_queue(Arc::new(move |event: NotificationQueueEvent| {
        sink.lock().expect("levels lock").push(event.count);
    }));
    r.set_event_handlers(events);
    (r, seen)
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
    assert_eq!(*seen.lock().expect("levels lock"), vec![1]);
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
    assert_eq!(*seen.lock().expect("levels lock"), vec![1, 0]);
}
