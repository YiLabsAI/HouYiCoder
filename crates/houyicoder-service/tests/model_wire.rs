//! Server-side wire slice for the /model select: the ModelSet request
//! switches the runner's active model id + returns the applied model, so the
//! TUI's /model pane select reaches the provider without importing the engine
//! crate. Drives the InProc frontend server with a ModelSet request and
//! asserts the runner's active_model changed + the reply echoes it.

use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope, RequestId};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::{Server, ServerIo};
use houyicoder_session::SessionStore;
use std::sync::Arc;

fn stub_runner() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let runner = Runner::with_shared_store(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "stub-model".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    (Arc::new(runner), session)
}

/// A unique temp settings path so ModelSet's persist_model_pick writes the
/// temp file (not the developer's real HOME settings) + the test stays
/// isolated from other crates' settings reads.
fn temp_settings() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("model-wire-{n}-{}.json", std::process::id()))
}

fn pair() -> (ServerIo, mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, client_rx) = mpsc::channel::<String>(256);
    (ServerIo::new(server_tx, server_rx), client_tx, client_rx)
}

/// The ModelSet request swaps the runner's active model id + replies with the
/// applied model (the /model pane select over the wire).
#[tokio::test]
async fn test_set_switches_runner_model() {
    let (runner, session) = stub_runner();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner.clone(),
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_settings_path(temp_settings());
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send(&mut client_tx, &Hello::local()).await;
    drop(recv(&mut client_rx).await); // server Hello

    let req = RequestEnvelope::new(
        RequestId(4),
        FrontendRequest::ModelSet {
            model: Some("glm-5.2".into()),
            effort: None,
            effort_toggled: false,
        },
    );
    send(&mut client_tx, &ClientFrame::Request(req)).await;

    // Drain until the ModelSet response lands on req_id 4.
    let mut got = String::new();
    for _ in 0..32 {
        let line = recv(&mut client_rx).await;
        if line.contains(r#""req_id":4"#) {
            got = line;
            break;
        }
    }
    assert!(!got.is_empty(), "no ModelSet response: {got}");
    // The reply echoes the applied model id.
    assert!(
        got.contains(r#""type":"model_result""#),
        "payload tag: {got}"
    );
    assert!(got.contains("glm-5.2"), "applied model echo: {got}");
    // The runner's active_model really changed.
    assert_eq!(runner.active_model(), "glm-5.2", "runner model switched");

    drop(client_tx);
    drop(handle.await);
}

async fn send(tx: &mut mpsc::Sender<String>, msg: &impl serde::Serialize) {
    let mut f = houyicoder_protocol::framing::encode(msg).expect("encode");
    if !f.ends_with('\n') {
        f.push('\n');
    }
    tx.send(f).await.unwrap();
}

async fn recv(rx: &mut mpsc::Receiver<String>) -> String {
    rx.next()
        .await
        .expect("server frame")
        .trim_end()
        .to_string()
}
