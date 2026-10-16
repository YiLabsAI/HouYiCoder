//! The reattaching connection's status contract. serve_session rebuilds a
//! Server from retained host state, so it is the one path that can drop what
//! the live connection reported: a test that builds a Server directly attaches
//! the facts itself and cannot see that.

use super::*;
use houyicoder_protocol::envelope::ResponsePayload;
use houyicoder_provider::ProviderFacts;

/// A reattached connection reports what the host was given. Without the attach
/// in serve_session this reads the environment instead, which is the defect the
/// carried facts exist to prevent.
#[tokio::test]
async fn test_reattach_reports_host_facts() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![CompletionResponse {
        output: vec![OutputItem::Text { text: "ok".into() }],
        usage: Usage::default(),
        model: "test".into(),
    }]));
    let mut runner = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    let event_sequencer = EventSequencer::new();
    event_sequencer.install_on(&mut runner);
    let gate: Arc<dyn houyicoder_permission::ModeGate> = Arc::new(DefaultModeGate::new());
    let carried = ProviderFacts {
        base_url: "https://team-gateway.example/compatible-mode/v1".into(),
        key_origin: Some("keychain houyicoder/dashscope-team".into()),
    };
    let host =
        Arc::new(SessionHost::new(SessionLeaseStore::new()).with_provider_facts(carried.clone()));
    host.insert(
        session,
        Arc::new(runner),
        event_sequencer,
        gate,
        Arc::new(Notify::new()),
    );

    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    let server_io = FrameCarrier::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));
    tokio::spawn(async move {
        drop(serve_session(host, session, server_io).await);
    });

    client.connect().await.expect("handshake");
    let req_id = RequestId(9);
    client
        .send_request(req_id, FrontendRequest::Status)
        .await
        .expect("status request");

    let mut reported = None;
    for _ in 0..64 {
        if let ServerFrame::Response(envelope) = client.next_frame().await.expect("server frame")
            && envelope.req_id == req_id
            && let ResponsePayload::Status(snapshot) = envelope.payload
        {
            reported = Some(snapshot);
            break;
        }
    }
    let snapshot = reported.expect("a status response");
    assert_eq!(
        snapshot.base_url, carried.base_url,
        "the reattached status carries what the host was given"
    );
    assert_eq!(snapshot.auth_token_source, carried.key_origin);
}
