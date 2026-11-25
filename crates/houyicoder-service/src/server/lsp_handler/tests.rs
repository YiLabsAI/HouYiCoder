//! The /lsp request contracts: the empty list, the toggle's persistence and
//! refusal paths, and the fresh list every reply carries. Driven over a real
//! frame carrier, so the payloads asserted are the ones the wire carries.

use std::env::temp_dir;
use std::fs;
use std::path::Path;
use std::process::id;
use std::sync::Arc;

use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::lsp::{LspConfigScope, LspServerState, ScopedLspServerConfig};
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use serde_json::Value;
use std::time::Duration;
use std::time::Instant;
use tokio::time::sleep;

use super::super::Server;
use super::super::frame_carrier::FrameCarrier;
use crate::lsp::LspConnections;
use crate::lsp::LspTimings;
use crate::lsp::NoSpawn;

fn plain_server() -> Server {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::new(
        store,
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    Server::new(
        Arc::new(runner),
        SessionId::new(),
        Arc::new(DefaultModeGate::new()),
    )
}

/// One manager holding a single server entry with the given switch state.
async fn manager_with(
    name: &str,
    scope: LspConfigScope,
    disabled: bool,
    locked: bool,
) -> Arc<LspConnections> {
    let manager = Arc::new(LspConnections::new(
        Arc::new(NoSpawn),
        Arc::from(Path::new("/work")),
        LspTimings::default(),
    ));
    let mut config = ScopedLspServerConfig::stdio("fake", Default::default());
    config.name = name.into();
    config.scope = scope;
    config.disabled = disabled;
    config.locked = locked;
    config
        .extension_to_language
        .insert(".rs".into(), "rust".into());
    manager.start_all(vec![config]).await;
    manager
}

fn send_line(tx: &mut mpsc::Sender<String>, frame: &impl serde::Serialize) {
    let mut s = encode(frame).expect("frame encodes");
    if !s.ends_with('\n') {
        s.push('\n');
    }
    tx.try_send(s).expect("line sends");
}

async fn recv_frame(rx: &mut mpsc::Receiver<String>) -> ServerFrame {
    serde_json::from_str(&rx.next().await.expect("a frame arrives")).expect("frame decodes")
}

/// Drive one server through a request and return its single reply payload.
async fn one_reply(server: Server, payload: FrontendRequest) -> ResponsePayload {
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    let req = ClientFrame::Request(RequestEnvelope::new(RequestId(1), payload));
    send_line(&mut client_tx, &req);
    let reply = match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => r.payload,
        other => panic!("expected a response, got {other:?}"),
    };
    handle.abort();
    reply
}

/// A session with no manager answers the list with an empty set, which is
/// the honest answer rather than an error.
#[tokio::test]
async fn test_lsp_status_list_empty() {
    let reply = one_reply(plain_server(), FrontendRequest::LspStatusList).await;
    match reply {
        ResponsePayload::LspStatus(rows) => assert!(rows.is_empty()),
        other => panic!("expected a status list, got {other:?}"),
    }
}

/// A toggle on a user-scope server persists to the settings file, so the
/// answer survives the session that made it, and the reply carries the
/// fresh row.
#[tokio::test]
async fn test_lsp_toggle_persists_user() {
    let dir = temp_dir().join(format!("lsp-handler-{}", id()));
    fs::create_dir_all(&dir).expect("the directory is created");
    let settings = dir.join("settings.json");
    fs::write(
        &settings,
        r#"{"lspServers":{"rust":{"command":"fake",
            "extensionToLanguage":{".rs":"rust"}}}}"#,
    )
    .expect("the settings file is written");
    let manager = manager_with("rust", LspConfigScope::User, false, false).await;
    let server = plain_server()
        .with_lsp(Some(manager))
        .with_settings_path(settings.clone());
    let reply = one_reply(
        server,
        FrontendRequest::LspSetEnabled {
            name: "rust".into(),
            enabled: false,
        },
    )
    .await;
    match reply {
        ResponsePayload::LspStatus(rows) => assert_eq!(rows.len(), 1),
        other => panic!("expected a status list, got {other:?}"),
    }
    let text = fs::read_to_string(&settings).expect("the settings file is read back");
    let value: Value = serde_json::from_str(&text).expect("the file stays json");
    assert_eq!(
        value["lspServers"]["rust"]["disabled"],
        Value::Bool(true),
        "the entry carries the disabled flag, saw {text}"
    );
    fs::remove_dir_all(&dir).ok();
}

/// A toggle of a project-scope entry applies in the session and leaves the
/// user settings untouched: only a user-scope answer outlives the session,
/// so a write here would silently override the project's own file.
#[tokio::test]
async fn test_lsp_toggle_project_only() {
    let dir = temp_dir().join(format!("lsp-handler-project-{}", id()));
    fs::create_dir_all(&dir).expect("the directory is created");
    let settings = dir.join("settings.json");
    let original =
        r#"{"lspServers":{"rust":{"command":"fake","extensionToLanguage":{".rs":"rust"}}}}"#;
    fs::write(&settings, original).expect("the settings file is written");
    let manager = manager_with("rust", LspConfigScope::Project, false, false).await;
    let server = plain_server()
        .with_lsp(Some(manager))
        .with_settings_path(settings.clone());
    let reply = one_reply(
        server,
        FrontendRequest::LspSetEnabled {
            name: "rust".into(),
            enabled: false,
        },
    )
    .await;
    match reply {
        ResponsePayload::LspStatus(rows) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].scope, LspConfigScope::Project);
            assert!(matches!(rows[0].state, LspServerState::Disabled { .. }));
        }
        other => panic!("expected a status list, got {other:?}"),
    }
    let text = fs::read_to_string(&settings).expect("the settings file is read back");
    assert_eq!(
        text.trim(),
        original,
        "a project-scope toggle does not rewrite the user settings"
    );
    fs::remove_dir_all(&dir).ok();
}

/// A switch the trust gate locked answers with the protocol error that names
/// the lock, so the pane shows why nothing changed.
#[tokio::test]
async fn test_lsp_toggle_locked_refused() {
    let manager = manager_with("gated", LspConfigScope::Project, true, true).await;
    let server = plain_server().with_lsp(Some(Arc::clone(&manager)));
    let reply = one_reply(
        server,
        FrontendRequest::LspSetEnabled {
            name: "gated".into(),
            enabled: true,
        },
    )
    .await;
    match reply {
        ResponsePayload::Error(e) => {
            assert!(
                e.message.contains("trusted"),
                "the refusal names the trust gate, saw {}",
                e.message
            );
        }
        other => panic!("expected an error, got {other:?}"),
    }
    let statuses = manager.statuses().await;
    assert!(
        matches!(statuses[0].state, LspServerState::Disabled { .. }),
        "the server stays off"
    );
}

/// A restart answers immediately with the current list while the launch
/// runs in the background; the row the pane polls then reports the failure,
/// so a wedged server never freezes the session waiting for its handshake.
#[tokio::test]
async fn test_lsp_restart_deferred() {
    let manager = manager_with("rust", LspConfigScope::User, false, false).await;
    let server = plain_server().with_lsp(Some(Arc::clone(&manager)));
    let reply = one_reply(
        server,
        FrontendRequest::LspRestart {
            name: "rust".into(),
        },
    )
    .await;
    match reply {
        ResponsePayload::LspStatus(rows) => assert_eq!(rows.len(), 1),
        other => panic!("expected a status list, got {other:?}"),
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let state = manager.statuses().await[0].state.clone();
        if let LspServerState::Failed { error } = &state {
            assert!(
                error.contains("did not start"),
                "the row names the launch failure, saw {error}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the failed launch reaches the row, saw {state:?}"
        );
        sleep(Duration::from_millis(5)).await;
    }
    manager.shutdown().await;
}
