//! Parent-side bridge for a child's permission ask: a PermissionRequest on the
//! bus is routed through the server's existing wire-approval flow, and the
//! decision is published back on the per-request response topic so the child
//! resumes. Mirrors the parent's own approval path (handle_approval); a
//! child ask is just another wire Permission the TUI answers.

use std::sync::Arc;

use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::ApprovalRequest;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, permission_response_topic,
};

use super::io::ServerIo;
use super::{Server, WireError};

impl Server {
    /// Wire the shared multi-agent bus so a child's permission ask reaches
    /// the wire-approval flow. None on the single-agent path.
    pub fn with_bus(mut self, bus: Option<Arc<AgentBus>>) -> Self {
        self.bus = bus;
        self
    }
}

/// Handle one child permission request: reconstruct the ApprovalRequest,
/// drive it through the wire-approval flow (the TUI answers), and publish the
/// decision on the per-request response topic the child is awaiting. On a
/// wire failure (client closed mid-ask) publish a deny so the child resumes
/// instead of hanging on a response that will not come.
pub(crate) async fn handle(
    server: &mut Server,
    io: &mut ServerIo,
    req: BusMessage,
) -> Result<(), WireError> {
    let BusMessage::PermissionRequest {
        child_id,
        call_id,
        tool,
        input,
        ..
    } = req
    else {
        // A non-request message on the permission topic: ignore.
        return Ok(());
    };
    let approval = ApprovalRequest::new(call_id.clone(), tool.clone(), input.clone());
    let (approved, updated_input) = match server.handle_approval(io, &approval).await {
        Ok(d) => (d.approved, d.updated_input),
        Err(e) => {
            // Fail-closed: publish a deny so the child resumes (rejected)
            // rather than hanging. The error still propagates so the caller
            // can surface a wire failure.
            if let Some(bus) = server.bus.as_ref() {
                bus.publish(
                    &permission_response_topic(&child_id, &call_id),
                    BusMessage::PermissionResponse {
                        call_id,
                        approved: false,
                        updated_input: None,
                        scope: "once".to_string(),
                    },
                );
            }
            return Err(e);
        }
    };
    if let Some(bus) = server.bus.as_ref() {
        bus.publish(
            &permission_response_topic(&child_id, &call_id),
            BusMessage::PermissionResponse {
                call_id,
                approved,
                updated_input,
                // The scope chosen at the TUI is applied by handle_approval's
                // consent routing on the shared gate; the child's resume only
                // reads approved + updated_input.
                scope: "once".to_string(),
            },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::io::ServerIo;
    use futures::SinkExt;
    use futures::StreamExt;
    use futures::channel::mpsc;
    use houyicoder_api::provider::ModelProvider;
    use houyicoder_core::agent::multi_agent::bus_types::{AgentBus, permission_response_topic};
    use houyicoder_protocol::envelope::{
        ClientFrame, ClientResponseEnvelope, ClientResponsePayload, ServerFrame,
    };
    use houyicoder_protocol::frontend::run::ApprovalDecision;
    use std::sync::Arc;

    /// A minimal server with a bus + a real gate, so handle() can route a
    /// child ask through the wire flow + publish the decision on the bus.
    fn server_with_bus() -> (Server, Arc<AgentBus>) {
        let sess_store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        )));
        let provider: Arc<dyn ModelProvider> =
            Arc::new(houyicoder_provider::FakeProvider::text("test"));
        let runner = houyicoder_core::agent::Runner::with_shared_store(
            sess_store,
            provider,
            houyicoder_core::agent::ToolRegistry::new(),
            houyicoder_core::agent::runner_config::RunnerConfig::default(),
        );
        let gate = Arc::new(houyicoder_permission::DefaultModeGate::new());
        let bus = Arc::new(AgentBus::new());
        let server = Server::new(Arc::new(runner), houyicoder_context::SessionId::new(), gate)
            .with_bus(Some(Arc::clone(&bus)));
        (server, bus)
    }

    fn encode_line(msg: &impl serde::Serialize) -> String {
        let mut f = houyicoder_protocol::framing::encode(msg).expect("encode");
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
        let mut io = ServerIo::new(server_tx, server_rx);
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
            agent_type: "explore".into(),
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
        let mut io = ServerIo::new(server_tx, server_rx);
        handle(
            &mut server,
            &mut io,
            BusMessage::Spawned {
                agent_id: "x".into(),
                subagent_type: "explore".into(),
            },
        )
        .await
        .expect("non-request ignored");
    }
}
