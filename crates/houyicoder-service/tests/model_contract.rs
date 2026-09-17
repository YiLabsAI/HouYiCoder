//! Model request contract across the frontend and server boundary: ModelSet
//! switches the runner's active model id + replies with the applied model, and
//! ModelInfo reports what the session is running. Both drive the InProc
//! frontend server, so the TUI's /model pane reaches the provider without
//! importing the engine crate.

use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::model::ModelChoice;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::{FrameCarrier, Server};
use houyicoder_session::SessionStore;
use std::sync::Arc;

fn stub_runner() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let runner = Runner::new(
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
    std::env::temp_dir().join(format!("model-contract-{n}-{}.json", std::process::id()))
}

fn pair() -> (FrameCarrier, mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, client_rx) = mpsc::channel::<String>(256);
    (
        FrameCarrier::new(server_tx, server_rx),
        client_tx,
        client_rx,
    )
}

/// A live frontend session over an in-process carrier, with the settings file
/// written before the server starts so the resolution chain reads it.
async fn start_session(settings_json: &str) -> Session {
    let (runner, session) = stub_runner();
    let settings = temp_settings();
    std::fs::write(&settings, settings_json).unwrap();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner.clone(),
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_settings_path(settings.clone());
    let handle = tokio::spawn(async move { server.serve(server_io).await });
    send(&mut client_tx, &Hello::local()).await;
    drop(recv(&mut client_rx).await); // server Hello
    Session {
        runner,
        settings,
        client_tx,
        client_rx,
        handle,
    }
}

struct Session {
    runner: Arc<Runner>,
    settings: std::path::PathBuf,
    client_tx: mpsc::Sender<String>,
    client_rx: mpsc::Receiver<String>,
    handle: tokio::task::JoinHandle<Result<(), houyicoder_protocol::error::ProtocolError>>,
}

impl Session {
    /// Send a request and return its response payload, skipping event frames.
    async fn request(&mut self, req_id: u64, payload: FrontendRequest) -> ResponsePayload {
        let id = RequestId(req_id);
        send(
            &mut self.client_tx,
            &ClientFrame::Request(RequestEnvelope::new(id, payload)),
        )
        .await;
        for _ in 0..32 {
            let line = recv(&mut self.client_rx).await;
            let Ok(frame) = serde_json::from_str::<ServerFrame>(&line) else {
                continue;
            };
            let ServerFrame::Response(envelope) = frame else {
                continue;
            };
            if envelope.req_id == id {
                return envelope.payload;
            }
        }
        panic!("no response for request {req_id}");
    }

    fn settings_json(&self) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(&self.settings).unwrap()).unwrap()
    }

    async fn close(self) {
        drop(self.client_tx);
        drop(self.handle.await);
    }
}

/// A Default pick applies the built-in default model, never an id read back
/// from settings. This session's settings file names another model, so a
/// Default resolution that consults it would strand the session on the model
/// the user just declined — and the reply would name a model nothing is
/// running.
#[tokio::test]
async fn test_default_ignores_prior_id() {
    let mut session = start_session(r#"{"model":{"id":"glm-5.2"}}"#).await;
    let payload = session
        .request(
            4,
            FrontendRequest::ModelSet {
                model: None,
                effort: None,
                effort_toggled: false,
                speed: None,
            },
        )
        .await;

    let ResponsePayload::ModelResult(result) = payload else {
        panic!("expected ModelResult, got {payload:?}");
    };
    assert_eq!(
        result.applied.id,
        houyicoder_config::DEFAULT_MODEL,
        "Default pick applies the constant, not the settings id"
    );
    assert!(
        matches!(result.selected, ModelChoice::Default),
        "the reply names the pick that was made: {:?}",
        result.selected
    );
    assert_eq!(
        session.runner.active_model(),
        houyicoder_config::DEFAULT_MODEL,
        "the runner really switched"
    );
    assert!(
        session.settings_json()["model"].get("id").is_none(),
        "a Default pick deletes the settings id: {}",
        session.settings_json()
    );
    session.close().await;
}

/// ModelInfo reports the live session model, not the settings model.id. A
/// session running another model keeps reporting that model, so the pane
/// cannot mark a settings row as applied while something else is running.
#[tokio::test]
async fn test_info_reports_live_choice() {
    let mut session = start_session(r#"{"model":{"id":"glm-5.2"}}"#).await;
    let payload = session.request(5, FrontendRequest::ModelInfo).await;

    let ResponsePayload::ModelInfo(catalog) = payload else {
        panic!("expected ModelInfo, got {payload:?}");
    };
    assert_eq!(
        catalog.applied.id, "stub-model",
        "the live session model is reported, not the settings id"
    );
    assert!(
        matches!(catalog.selected, ModelChoice::Explicit { ref id } if id == "stub-model"),
        "the selection expresses what is running: {:?}",
        catalog.selected
    );
    assert_eq!(
        catalog.resolved_default.id,
        houyicoder_config::DEFAULT_MODEL,
        "the Default row still resolves to the constant"
    );
    session.close().await;
}

/// The ModelSet request swaps the runner's active model id + replies with the
/// applied model (the /model pane select across the server boundary).
#[tokio::test]
async fn test_set_switches_runner_model() {
    let mut session = start_session("{}").await;
    let payload = session
        .request(
            6,
            FrontendRequest::ModelSet {
                model: Some("glm-5.2".into()),
                effort: None,
                effort_toggled: false,
                speed: None,
            },
        )
        .await;

    let ResponsePayload::ModelResult(result) = payload else {
        panic!("expected ModelResult, got {payload:?}");
    };
    assert_eq!(result.applied.id, "glm-5.2", "applied model echo");
    assert_eq!(
        session.runner.active_model(),
        "glm-5.2",
        "runner model switched"
    );
    assert_eq!(
        session.settings_json()["model"]["id"],
        "glm-5.2",
        "the pick is written for the next session"
    );
    session.close().await;
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
