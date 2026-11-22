//! Tests for the live turn-event stream.

use std::sync::Arc;

use super::{TURN_EVENT_CAPACITY, TurnEvents, TurnStreamEvent};
use houyicoder_api::agent_event::{
    EventHandler, ResponseStreamEvent, ToolExecutionEvent, UserNoticeEvent,
};
use houyicoder_api::provider::ModelProvider;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;

fn delta(text: &str) -> ResponseStreamEvent {
    ResponseStreamEvent::AssistantTextDelta { text: text.into() }
}

/// Each domain reaches a subscriber in the order the run produced it.
#[tokio::test]
async fn test_events_keep_order() {
    let events = TurnEvents::new();
    let mut stream = events.subscribe();
    let response = delta("hi");
    let progress = ToolExecutionEvent::Progress {
        call_id: "call-1".into(),
        elapsed_secs: 2,
        timeout_secs: Some(30),
        tail: vec!["partial".into()],
    };
    let notice = UserNoticeEvent {
        message: "heads up".into(),
    };
    events.handle(response.clone());
    events.handle(progress.clone());
    events.handle(notice.clone());

    assert_eq!(
        stream.recv().await,
        Some(TurnStreamEvent::Response(response))
    );
    assert_eq!(
        stream.recv().await,
        Some(TurnStreamEvent::ToolProgress(progress))
    );
    assert_eq!(stream.recv().await, Some(TurnStreamEvent::Notice(notice)));
    assert_eq!(stream.take_lagged(), 0);
}

/// A subscriber that stops reading keeps only the newest capacity of events
/// and reports the count it lost, so a slow connection cannot grow a backlog.
#[tokio::test]
async fn test_slow_stream_drops_oldest() {
    let events = TurnEvents::new();
    let mut stream = events.subscribe();
    let total = TURN_EVENT_CAPACITY + 8;
    for i in 0..total {
        events.handle(delta(&format!("{i}")));
    }

    let first = stream.recv().await.expect("an event survives the overflow");
    assert_eq!(
        first,
        TurnStreamEvent::Response(delta("8")),
        "the oldest events were the ones dropped"
    );
    assert_eq!(stream.take_lagged(), 8);
    assert_eq!(stream.take_lagged(), 0, "reading the count resets it");
}

/// Events a forwarding source skipped count as this connection's loss, so a
/// turn damaged upstream still keeps its durable copy.
#[test]
fn test_upstream_lag_counts() {
    let events = TurnEvents::new();
    let mut stream = events.subscribe();
    events.note_upstream_lag(5);
    assert_eq!(stream.take_lagged(), 5);
    assert_eq!(stream.take_lagged(), 0);
}

/// Publishing with no subscriber reports the miss instead of panicking, and a
/// subscriber that arrives later starts from the next event.
#[test]
fn test_publish_without_subscriber() {
    let events = TurnEvents::new();
    assert!(!events.publish(TurnStreamEvent::Response(delta("x"))));
    let _stream = events.subscribe();
    assert!(events.publish(TurnStreamEvent::Response(delta("x"))));
}

/// A stream with no producer left ends instead of waiting forever, and a
/// non-blocking read reports the same end.
#[tokio::test]
async fn test_stream_ends_with_producer() {
    let events = TurnEvents::new();
    let mut stream = events.subscribe();
    drop(events);
    assert_eq!(stream.recv().await, None);
    assert!(stream.try_recv().is_none());
}

/// Installing the stream routes all three live domains of the runner through
/// it without disturbing the handlers it does not own.
#[test]
fn test_installs_three_domains() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("x"));
    let mut runner = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let events = TurnEvents::new();
    events.install_on(&mut runner);

    let handlers = runner.event_handlers();
    assert!(handlers.response_stream_handler().is_some());
    assert!(handlers.tool_execution_handler().is_some());
    assert!(handlers.user_notice_handler().is_some());
}
