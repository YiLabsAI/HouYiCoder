//! The /mcp request contracts: the status list, the reconnect reply, and
//! the toggle's persistence and refusal paths. Driven over a real frame
//! carrier, so the payloads asserted are the ones the wire carries.

use std::env::temp_dir;
use std::fs;
use std::path::Path;
use std::process::id;
use std::sync::Arc;

use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::mcp::{
    McpConfigScope, McpConnectionState, McpServerStatusView, McpTransportConfig,
    ScopedMcpServerConfig,
};
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use serde_json::Value;
use tokio::runtime::Handle;

use super::super::Server;
use super::super::frame_carrier::FrameCarrier;
use crate::mcp::McpConnections;
use crate::mcp::McpTimings;
use crate::mcp::NoSpawn;

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

fn manager_with(rows: Vec<McpServerStatusView>, trusted: bool) -> Arc<McpConnections> {
    let manager = Arc::new(McpConnections::new(
        Arc::new(NoSpawn),
        Arc::from(Path::new("/work")),
        McpTimings::default(),
    ));
    {
        let runtime = Handle::current();
        let _entered = runtime.enter();
        let start = Arc::clone(&manager);
        let configs = rows
            .into_iter()
            .map(|row| ScopedMcpServerConfig {
                name: row.name,
                scope: row.scope,
                transport: McpTransportConfig::stdio("fake", vec![]),
                disabled: true,
                tool_timeout_ms: 5_000,
            })
            .collect();
        tokio::spawn(async move {
            start.start_all(configs, trusted).await;
        });
    }
    manager
}

fn status_row(name: &str, scope: McpConfigScope, state: McpConnectionState) -> McpServerStatusView {
    McpServerStatusView {
        name: name.into(),
        scope,
        state,
        server_version: None,
        tool_count: 0,
        resource_count: 0,
        has_instructions: false,
        recent_stderr: Vec::new(),
        detail: Default::default(),
    }
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
async fn test_mcp_status_list_empty() {
    let reply = one_reply(plain_server(), FrontendRequest::McpStatusList).await;
    match reply {
        ResponsePayload::McpStatus(rows) => assert!(rows.is_empty()),
        other => panic!("expected a status list, got {other:?}"),
    }
}

/// The list carries the manager's rows, and the reconnect request answers
/// with a fresh list rather than a bare acknowledgement, so the pane's rows
/// and the change that caused them land on one reply.
#[tokio::test]
async fn test_mcp_reconnect_replies_statuses() {
    let manager = manager_with(
        vec![status_row(
            "srv",
            McpConfigScope::User,
            McpConnectionState::Failed {
                error: "the port refused".into(),
            },
        )],
        true,
    );
    let server = plain_server().with_mcp(Some(manager));
    let reply = one_reply(server, FrontendRequest::McpReconnect { name: "srv".into() }).await;
    match reply {
        ResponsePayload::McpStatus(rows) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].name, "srv");
        }
        other => panic!("expected a status list, got {other:?}"),
    }
}

/// A toggle the trust gate refuses answers with the protocol error that
/// names the lock, so the pane shows why nothing changed.
#[tokio::test]
async fn test_mcp_toggle_locked_refused() {
    let manager = manager_with(
        vec![status_row(
            "gated",
            McpConfigScope::Project,
            McpConnectionState::Disabled {
                reason: "the workspace is not trusted".into(),
            },
        )],
        false,
    );
    let server = plain_server().with_mcp(Some(Arc::clone(&manager)));
    let reply = one_reply(
        server,
        FrontendRequest::McpSetEnabled {
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
        matches!(statuses[0].state, McpConnectionState::Disabled { .. }),
        "the server stays off"
    );
}

/// A toggle on a user-scope server persists to the settings file, so the
/// answer survives the session that made it.
#[tokio::test]
async fn test_mcp_toggle_persists_user() {
    let dir = temp_dir().join(format!("mcp-handler-{}", id()));
    fs::create_dir_all(&dir).expect("the directory is created");
    let settings = dir.join("settings.json");
    fs::write(&settings, r#"{"mcpServers":{"srv":{"command":"fake"}}}"#)
        .expect("the settings file is written");
    let manager = manager_with(
        vec![status_row(
            "srv",
            McpConfigScope::User,
            McpConnectionState::Pending,
        )],
        true,
    );
    let server = plain_server()
        .with_mcp(Some(manager))
        .with_settings_path(settings.clone());
    let reply = one_reply(
        server,
        FrontendRequest::McpSetEnabled {
            name: "srv".into(),
            enabled: false,
        },
    )
    .await;
    assert!(
        matches!(reply, ResponsePayload::McpStatus(_)),
        "the successful toggle answers with the list"
    );
    let text = fs::read_to_string(&settings).expect("the settings file is read back");
    let value: Value = serde_json::from_str(&text).expect("the file stays json");
    assert_eq!(
        value["mcpServers"]["srv"]["disabled"],
        Value::Bool(true),
        "the entry carries the disabled flag, saw {text}"
    );
    fs::remove_dir_all(&dir).ok();
}

/// A toggle on a server the settings file does not own applies in the
/// session and answers with the list, leaving no file to write.
#[tokio::test]
async fn test_mcp_toggle_env_scoped() {
    let manager = manager_with(
        vec![status_row(
            "srv",
            McpConfigScope::Env,
            McpConnectionState::Pending,
        )],
        true,
    );
    let server = plain_server().with_mcp(Some(manager));
    let reply = one_reply(
        server,
        FrontendRequest::McpSetEnabled {
            name: "srv".into(),
            enabled: true,
        },
    )
    .await;
    match reply {
        ResponsePayload::McpStatus(rows) => assert_eq!(rows.len(), 1),
        other => panic!("expected a status list, got {other:?}"),
    }
}
