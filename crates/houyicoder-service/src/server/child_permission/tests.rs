//! Tests for this module.

use super::*;
use crate::server::frame_carrier::FrameCarrier;
use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, ChildDescriptor, ChildRunMode, permission_response_topic,
};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponseEnvelope, ClientResponsePayload, ServerFrame,
};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::run::ApprovalDecision;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::sync::Arc;

/// A minimal server with a bus + a real gate, so handle() can route a
/// child ask through the wire flow + publish the decision on the bus.
fn server_with_bus() -> (Server, Arc<AgentBus>) {
    let sess_store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let runner = Runner::new(
        sess_store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let gate = Arc::new(DefaultModeGate::new());
    let bus = Arc::new(AgentBus::new());
    let server =
        Server::new(Arc::new(runner), SessionId::new(), gate).with_bus(Some(Arc::clone(&bus)));
    (server, bus)
}

fn encode_line(msg: &impl serde::Serialize) -> String {
    let mut f = encode(msg).expect("encode");
    if !f.ends_with('\n') {
        f.push('\n');
    }
    f
}

/// handle() routes a child PermissionRequest through the wire flow and
/// publishes the TUI's decision on the per-request response topic, so the
/// awaiting child resumes. Pins the bus-to-wire-to-bus bridge.
#[tokio::test]
async fn test_handle_routes_ask_response() {
    let (mut server, bus) = server_with_bus();
    let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    let mut resp_rx = bus.subscribe(&permission_response_topic("c1", "call-1"));
    // Wire-mock feeder: read the Permission ask, send back an approve.
    let feeder = tokio::spawn(async move {
        let mut ask_id = None;
        for _ in 0..16 {
            let line = client_rx.next().await.expect("server frame");
            if let Ok(ServerFrame::Request(req)) = serde_json::from_str(&line) {
                ask_id = Some(req.req_id);
                break;
            }
        }
        let ask_id = ask_id.expect("permission ask sent");
        let resp = ClientFrame::Response(ClientResponseEnvelope::new(
            ask_id,
            ClientResponsePayload::Permission(ApprovalDecision {
                call_id: "call-1".into(),
                approved: true,
                updated_input: None,
                scope: "once".to_string(),
            }),
        ));
        client_tx
            .send(encode_line(&resp))
            .await
            .expect("send response");
    });
    let req = BusMessage::PermissionRequest {
        child_id: "c1".into(),
        subagent_type: "explore".into(),
        call_id: "call-1".into(),
        tool: "bash".into(),
        input: serde_json::json!({"command": "echo hi"}),
    };
    handle(&mut server, &mut io, req).await.expect("handle ok");
    feeder.await.expect("feeder done");
    match resp_rx.recv().await.expect("response published") {
        BusMessage::PermissionResponse { approved, .. } => assert!(approved),
        other => panic!("expected PermissionResponse, got {other:?}"),
    }
}

/// The permission-request topic is request-only; a stray non-request
/// message is ignored (no panic, no wire ask, no response).
#[tokio::test]
async fn test_handle_ignores_non_request() {
    let (mut server, _bus) = server_with_bus();
    let (_client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, _client_rx) = mpsc::channel::<String>(8);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    handle(
        &mut server,
        &mut io,
        BusMessage::Spawned {
            child: ChildDescriptor::new("x", "explore", ChildRunMode::Foreground),
        },
    )
    .await
    .expect("non-request ignored");
}
