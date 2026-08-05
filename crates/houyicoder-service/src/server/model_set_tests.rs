//! ModelSet dispatch handler -- inline (src/) tests. The dispatch handler is
//! async + needs ServerIo, which is testable inline via ServerIo::new, so the
//! tests stay here (mirroring the rename_session_tests precedent) to count
//! toward --lib diff-cov (make check's --lib lcov cannot see tests/ coverage).
//! Covers the Some-id swap, the None-id keep-current branch, + effort
//! pass-through to the reply. The Default-sentinel resolution, per-model
//! effort persistence, and sidecar write land in a later task; here the wire
//! shape is what is verified.

#![cfg(test)]

use super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{
    ClientFrame, ModelApplied, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::llm::EffortLevel;
use houyicoder_session::SessionStore;
use std::sync::Arc;

fn stub_runner() -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(Runner::with_shared_store(
        store,
        Arc::new(houyicoder_provider::FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig {
            model: "stub-model".into(),
            ..RunnerConfig::default()
        },
    ))
}

/// A unique temp settings path so a ModelSet's persist_model_pick writes the
/// temp file (not the developer's real HOME settings) + the test stays
/// isolated. The file need not pre-exist; update_settings creates it.
fn temp_settings(slug: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("model-set-{slug}-{n}-{}.json", std::process::id()))
}

fn send_line(tx: &mut mpsc::Sender<String>, frame: &impl serde::Serialize) {
    let mut s = houyicoder_protocol::framing::encode(frame).unwrap();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    tx.try_send(s).unwrap();
}

async fn recv_frame(rx: &mut mpsc::Receiver<String>) -> ServerFrame {
    serde_json::from_str(&rx.next().await.unwrap()).expect("frame decodes")
}

/// A Some(model) ModelSet swaps the runner's active model + replies with the
/// applied id. effort None passes through (no effort parameter sent).
#[tokio::test]
async fn test_model_set_some_swaps() {
    let runner = stub_runner();
    let session = SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = ServerIo::new(server_tx, server_rx);
    let server = Server::new(
        runner.clone(),
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_settings_path(temp_settings("set"));
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(1),
            FrontendRequest::ModelSet {
                model: Some("glm-5.2".into()),
                effort: None,
                effort_toggled: false,
            },
        )),
    );
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::ModelResult(ModelApplied { model, effort }) => {
                assert_eq!(model, "glm-5.2", "reply carries the applied id");
                assert!(effort.is_none(), "effort None passes through");
            }
            other => panic!("expected ModelResult, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
    assert_eq!(runner.active_model(), "glm-5.2", "runner model switched");
    handle.abort();
}

/// A None model ModelSet resolves the Default sentinel (settings → DEFAULT)
/// and sets the session effort. The reply carries the actually-applied effort
/// (resolved through the chain), not the picker's request.
#[tokio::test]
async fn test_set_none_resolves_sentinel() {
    let runner = stub_runner();
    let session = SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = ServerIo::new(server_tx, server_rx);
    let server = Server::new(
        runner.clone(),
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_settings_path(temp_settings("set"));
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(2),
            FrontendRequest::ModelSet {
                model: None,
                effort: Some(EffortLevel::High),
                effort_toggled: true,
            },
        )),
    );
    let resolved = houyicoder_config::resolve_model();
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::ModelResult(ModelApplied { model, effort: _ }) => {
                assert_eq!(
                    model, resolved,
                    "None resolves the Default sentinel through the settings→DEFAULT chain"
                );
                // The session effort is set; the reply's effort is whatever
                // the chain resolves for the resolved model (None if the model
                // speaks no effort dialect — the honest value).
            }
            other => panic!("expected ModelResult, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
    assert_eq!(
        runner.active_model(),
        resolved,
        "runner model swapped to the resolved sentinel"
    );
    assert_eq!(
        runner.active_effort(),
        Some(EffortLevel::High),
        "session effort set from the picker"
    );
    handle.abort();
}

/// A Some model with an effort the model supports echoes the applied effort
/// back (qwen3 speaks the qwen3 dialect, so High is honored).
#[tokio::test]
async fn test_model_set_effort_applied() {
    let runner = stub_runner();
    let session = SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = ServerIo::new(server_tx, server_rx);
    let server = Server::new(
        runner.clone(),
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_settings_path(temp_settings("set"));
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(3),
            FrontendRequest::ModelSet {
                model: Some("qwen3.7-max".into()),
                effort: Some(EffortLevel::High),
                effort_toggled: true,
            },
        )),
    );
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::ModelResult(ModelApplied { model, effort }) => {
                assert_eq!(model, "qwen3.7-max");
                assert_eq!(effort, Some(EffortLevel::High), "qwen3 supports effort");
            }
            other => panic!("expected ModelResult, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
    handle.abort();
}

/// A ModelInfo request projects the settings model section into the pane
/// snapshot. A temp settings.json with a catalog + active id round-trips back
/// as a ModelInfo(catalog) reply; the catalog preserves order + the fields.
#[tokio::test]
async fn test_info_projects_settings_catalog() {
    use houyicoder_protocol::envelope::ResponsePayload;
    use houyicoder_protocol::frontend::model::ModelCatalog;
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let path = std::env::temp_dir().join(format!("model-info-{}.json", std::process::id()));
    std::fs::write(
        &path,
        r#"{"model":{"id":"qwen3.7-max","catalog":[{"id":"qwen3.7-max","display_name":"Max","description":"most capable"},{"id":"glm-5.2","display_name":"Fable"}]}}"#,
    )
    .unwrap();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = ServerIo::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_settings_path(path.clone());
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(
        &mut client_tx,
        &houyicoder_protocol::handshake::Hello::local(),
    );
    drop(client_rx.next().await);
    send_line(
        &mut client_tx,
        &houyicoder_protocol::envelope::ClientFrame::Request(
            houyicoder_protocol::envelope::RequestEnvelope::new(
                houyicoder_protocol::envelope::RequestId(7),
                houyicoder_protocol::frontend::FrontendRequest::ModelInfo,
            ),
        ),
    );
    let mut got = String::new();
    for _ in 0..32 {
        let line = client_rx.next().await.unwrap();
        if line.contains(r#""req_id":7"#) {
            got = line;
            break;
        }
    }
    assert!(!got.is_empty(), "no ModelInfo response: {got}");
    let frame: houyicoder_protocol::envelope::ServerFrame =
        serde_json::from_str(got.trim_end()).unwrap();
    let houyicoder_protocol::envelope::ServerFrame::Response(r) = frame else {
        panic!("expected response");
    };
    match r.payload {
        ResponsePayload::ModelInfo(ModelCatalog {
            active_id, catalog, ..
        }) => {
            assert_eq!(active_id.as_deref(), Some("qwen3.7-max"));
            assert_eq!(catalog.len(), 2, "catalog order preserved");
            assert_eq!(catalog[0].id, "qwen3.7-max");
            assert_eq!(catalog[0].display_name.as_deref(), Some("Max"));
            assert_eq!(catalog[1].display_name.as_deref(), Some("Fable"));
        }
        other => panic!("expected ModelInfo, got {other:?}"),
    }
    drop(std::fs::remove_file(&path));
    handle.abort();
}
