//! ACP server contract: one turn end to end over the base JSON-RPC dialect
//! with the real runner driving the run. The client sends initialize, then
//! session/prompt; the server streams session/update notifications for the
//! turn events and returns a PromptResponse with the stop reason. Same path
//! a pipe carrier would serve — the adapter handles the stateless methods,
//! AcpServer owns the runner + the half-live turn state machine.

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::acp_wire::{AcpRequest, PromptResponse};
use houyicoder_protocol::acpx::AcpxCapabilities;
use houyicoder_protocol::framing::encode;
use houyicoder_provider::FakeProvider;
use houyicoder_service::acp_adapter::AcpAdapter;
use houyicoder_service::acp_serve::AcpIo;
use houyicoder_service::acp_server::AcpServer;
use houyicoder_service::lifecycle::SessionLeaseStore;
use houyicoder_session::SessionStore;
use std::sync::Arc;

/// Build a minimal runner over a stub provider that returns one text reply.
fn stub_runner() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("hello from stub"));
    let tools = ToolRegistry::new();
    let runner = Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    (Arc::new(runner), session)
}

fn pair() -> (AcpIo, mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    (AcpIo::new(server_tx, server_rx), client_tx, client_rx)
}

async fn send_line(tx: &mut mpsc::Sender<String>, msg: &impl serde::Serialize) {
    let mut frame = encode(msg).expect("encode");
    if !frame.ends_with('\n') {
        frame.push('\n');
    }
    tx.send(frame).await.expect("client sends frame");
}

async fn recv_line(rx: &mut mpsc::Receiver<String>) -> String {
    let line = rx.next().await.expect("server sent a frame");
    line.strip_suffix('\n').unwrap_or(&line).to_string()
}

#[tokio::test]
async fn test_prompt_streams_updates_replies() {
    let (runner, session) = stub_runner();
    let session_str = session.to_string();
    let store = SessionLeaseStore::new();
    let adapter = Arc::new(AcpAdapter::new(AcpxCapabilities::default(), 1, store));
    let (mut io, mut client_tx, mut client_rx) = pair();
    let server = AcpServer::new(adapter, runner, session);
    let handle = tokio::spawn(async move { server.serve(&mut io).await });

    // initialize
    send_line(
        &mut client_tx,
        &AcpRequest::new(1, "initialize", serde_json::json!({})),
    )
    .await;
    let init_reply = recv_line(&mut client_rx).await;
    assert!(
        init_reply.contains(r#""protocolVersion":1"#),
        "{init_reply}"
    );

    // session/prompt: one text block.
    let prompt = serde_json::json!({
        "sessionId": session_str,
        "prompt": [{"type": "text", "text": "hi"}],
    });
    send_line(
        &mut client_tx,
        &AcpRequest::new(2, "session/prompt", prompt),
    )
    .await;

    // Drain frames until the PromptResponse reply arrives on id 2. The turn
    // events arrive as session/update notifications (no id); the final
    // outcome arrives as a response paired to id 2.
    let mut saw_update = false;
    let mut prompt_reply = None;
    for _ in 0..64 {
        let line = recv_line(&mut client_rx).await;
        if line.contains(r#""id":2"#) {
            prompt_reply = Some(line);
            break;
        }
        if line.contains(r#""method":"session/update""#) {
            saw_update = true;
        }
    }
    let reply = prompt_reply.expect("got a prompt reply");
    assert!(saw_update, "expected at least one session/update");
    assert!(reply.contains(r#""stopReason":"end_turn""#), "{reply}");

    drop(client_tx);
    drop(handle.await);
}

#[tokio::test]
async fn test_prompt_response_round_trips() {
    // Sanity: the PromptResponse shape the server sends is the wire shape
    // a stock ACP client decodes.
    let resp = PromptResponse {
        stop_reason: houyicoder_protocol::frontend::run::StopReason::EndTurn,
        meta: None,
    };
    let j = serde_json::to_string(&serde_json::to_value(&resp).unwrap()).unwrap();
    assert!(j.contains(r#""stopReason":"end_turn""#), "{j}");
}

/// A provider whose stream emits the given LlmEvents then never yields again,
/// so the drive loop sits in the streaming select! until abort fires. Mirrors
/// the engine's HangingProvider (pub(crate) there) so this contract test can
/// exercise the wire-side cancel path without a real latency window.
struct HangingProvider {
    events: Vec<houyicoder_protocol::llm::LlmEvent>,
}

impl HangingProvider {
    fn new(events: Vec<houyicoder_protocol::llm::LlmEvent>) -> Self {
        Self { events }
    }
}

impl ModelProvider for HangingProvider {
    fn complete(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        houyicoder_protocol::llm::CompletionResponse,
                        houyicoder_protocol::llm::ProviderError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async {
            Ok(houyicoder_protocol::llm::CompletionResponse {
                output: vec![],
                usage: houyicoder_protocol::llm::Usage::default(),
                model: "test".into(),
            })
        })
    }
    fn stream(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> std::pin::Pin<
        Box<
            dyn futures::Stream<
                    Item = Result<
                        houyicoder_protocol::llm::LlmEvent,
                        houyicoder_protocol::llm::ProviderError,
                    >,
                > + Send
                + '_,
        >,
    > {
        let prefix = futures::stream::iter(self.events.clone().into_iter().map(Ok));
        let tail = futures::stream::pending();
        Box::pin(prefix.chain(tail))
    }
    fn capabilities(&self) -> houyicoder_protocol::llm::ModelCapabilities {
        houyicoder_protocol::llm::ModelCapabilities::default()
    }
}

#[tokio::test]
async fn test_cancel_during_run_cancelled() {
    use houyicoder_protocol::llm::LlmEvent;
    let provider: Arc<dyn ModelProvider> =
        Arc::new(HangingProvider::new(vec![LlmEvent::TextDelta {
            id: "t1".into(),
            text: "partial".into(),
        }]));
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let session_str = session.to_string();
    let tools = ToolRegistry::new();
    let runner = Arc::new(Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));
    let (mut io, mut client_tx, mut client_rx) = pair();
    let server = AcpServer::new(adapter, runner, session);
    let handle = tokio::spawn(async move { server.serve(&mut io).await });

    send_line(
        &mut client_tx,
        &AcpRequest::new(1, "initialize", serde_json::json!({})),
    )
    .await;
    drop(recv_line(&mut client_rx).await); // initialize reply

    let prompt = serde_json::json!({
        "sessionId": session_str,
        "prompt": [{"type": "text", "text": "hi"}],
    });
    send_line(
        &mut client_tx,
        &AcpRequest::new(2, "session/prompt", prompt),
    )
    .await;

    // Let the run enter the streaming select! (the pending tail). A yield is
    // enough — the run is hung in the stream's pending tail, no frames yet.
    tokio::task::yield_now().await;

    // Send the cancel notification. The concurrent reader in handle_prompt
    // aborts the run; it resolves Interrupted; the prompt replies cancelled.
    let cancel = houyicoder_protocol::acp_wire::AcpNotification::new(
        "session/cancel",
        serde_json::json!({"sessionId": session_str}),
    );
    send_line(&mut client_tx, &cancel).await;

    // Drain until the prompt reply lands on id 2.
    let mut reply = None;
    for _ in 0..32 {
        let line = recv_line(&mut client_rx).await;
        if line.contains(r#""id":2"#) {
            reply = Some(line);
            break;
        }
    }
    let reply = reply.expect("got a cancelled prompt reply");
    assert!(
        reply.contains(r#""stopReason":"cancelled""#),
        "expected cancelled stop reason, got: {reply}"
    );

    drop(client_tx);
    drop(handle.await);
}
