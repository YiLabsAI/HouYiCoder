//! Tests for this module.

use super::*;
use houyicoder_api::provider::ModelProvider;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;

/// A delta handed to the sink reaches the serve loop in production order.
#[tokio::test]
async fn test_sink_forwards_deltas() {
    let (sink, mut stream) = AcpLiveStream::channel();
    sink.handle(ResponseStreamEvent::AssistantTextDelta { text: "hel".into() });
    sink.handle(ResponseStreamEvent::AssistantTextDelta { text: "lo".into() });

    assert_eq!(
        stream.recv().await,
        Some(ResponseStreamEvent::AssistantTextDelta { text: "hel".into() })
    );
    assert_eq!(
        stream.recv().await,
        Some(ResponseStreamEvent::AssistantTextDelta { text: "lo".into() })
    );
    drop(sink);
    assert_eq!(
        stream.recv().await,
        None,
        "the closed queue ends the stream"
    );
}

/// try_recv takes a queued delta without waiting and returns None when the
/// channel is empty, so the serve loop can drain the tail after a run
/// resolves without blocking.
#[tokio::test]
async fn test_sink_try_recv_drains() {
    let (sink, mut stream) = AcpLiveStream::channel();
    sink.handle(ResponseStreamEvent::AssistantTextDelta { text: "a".into() });
    sink.handle(ResponseStreamEvent::AssistantTextDelta { text: "b".into() });
    assert_eq!(
        stream.try_recv(),
        Some(ResponseStreamEvent::AssistantTextDelta { text: "a".into() })
    );
    assert_eq!(
        stream.try_recv(),
        Some(ResponseStreamEvent::AssistantTextDelta { text: "b".into() })
    );
    assert!(stream.try_recv().is_none(), "channel drained");
}

/// Installing the sink makes it the runner's response-stream destination.
#[test]
fn test_sink_installs_on_runner() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("x"));
    let mut runner = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let (sink, _stream) = AcpLiveStream::channel();
    sink.install_on(&mut runner);
    assert!(runner.event_handlers().response_stream_handler().is_some());
}
