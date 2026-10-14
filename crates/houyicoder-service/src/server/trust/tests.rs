//! Tests for this module.

use super::*;

/// A project path not recorded in settings is Untrusted; recording it
/// flips it to Acknowledged (persisted in user-level settings). A None
/// project path is Trusted (a non-project session, no project source).
#[test]
fn test_compute_trust_state() {
    let dir = std::env::temp_dir().join(format!("trust-compute-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).unwrap();
    let settings = dir.join("settings.json");
    let proj = dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();

    assert_eq!(
        compute_trust_state(&settings, Some(&proj)),
        TrustState::Untrusted,
        "unrecorded project is untrusted"
    );
    houyicoder_config::persist_project_trust(&settings, &proj).unwrap();
    assert_eq!(
        compute_trust_state(&settings, Some(&proj)),
        TrustState::Acknowledged,
        "recorded project is acknowledged"
    );

    // None project path: a non-project session, trusted (nothing to gate).
    assert_eq!(
        compute_trust_state(&settings, None),
        TrustState::Trusted,
        "no project path is trusted"
    );

    drop(std::fs::remove_dir_all(&dir));
}

/// A minimal server bound to a temp settings path + a project path that
/// is not yet trusted, so ensure_trust fires the prompt. Mirrors the
/// ask_wait_server helper in server/approval.rs.
fn trust_prompt_server(settings: std::path::PathBuf, project: std::path::PathBuf) -> super::Server {
    use std::sync::Arc;
    let sess_store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(houyicoder_provider::FakeProvider::text("test"));
    let runner = houyicoder_core::agent::Runner::new(
        sess_store,
        provider,
        houyicoder_core::agent::ToolRegistry::new(),
        houyicoder_core::agent::runner_config::RunnerConfig::default(),
    );
    super::Server::new(
        Arc::new(runner),
        houyicoder_context::SessionId::new(),
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_settings_path(settings)
    .with_project_path(project)
}

fn encode_line(msg: &impl serde::Serialize) -> String {
    let mut f = houyicoder_protocol::framing::encode(msg).expect("encode");
    if !f.ends_with('\n') {
        f.push('\n');
    }
    f
}

/// An untrusted project fires the TrustPrompt; the client accepts; the
/// path is persisted in user-level settings and ensure_trust returns
/// Acknowledged (so the prompt will not repeat next launch).
#[tokio::test]
async fn test_ensure_trust_accepts() {
    use futures::SinkExt;
    use futures::StreamExt;
    use futures::channel::mpsc;
    use houyicoder_protocol::envelope::{
        ClientFrame, ClientResponseEnvelope, ClientResponsePayload, ServerFrame,
    };

    let dir = std::env::temp_dir().join(format!("trust-accept-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).unwrap();
    let settings = dir.join("settings.json");
    let proj = dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();

    let mut server = trust_prompt_server(settings.clone(), proj.clone());
    let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
    let mut io = super::super::frame_carrier::FrameCarrier::new(server_tx, server_rx);

    let feeder = tokio::spawn(async move {
        // Read the TrustPrompt ask to learn its req_id, then accept.
        let mut ask_id = None;
        for _ in 0..16 {
            let line = client_rx.next().await.expect("server frame");
            if let Ok(ServerFrame::Request(req)) = serde_json::from_str(&line) {
                ask_id = Some(req.req_id);
                break;
            }
        }
        let ask_id = ask_id.expect("trust prompt sent");
        let resp = ClientFrame::Response(ClientResponseEnvelope::new(
            ask_id,
            ClientResponsePayload::TrustAccept(houyicoder_protocol::frontend::trust::TrustAccept {
                accepted: true,
            }),
        ));
        client_tx
            .send(encode_line(&resp))
            .await
            .expect("send accept");
    });

    let state = server
        .ensure_trust(&mut io)
        .await
        .expect("accept resolves trust");
    feeder.await.expect("feeder done");
    assert_eq!(state, TrustState::Acknowledged, "accept acknowledges");
    // The path is now persisted so the next session skips the prompt.
    assert!(
        houyicoder_config::is_path_trusted(&settings, &proj),
        "accept persists the path"
    );

    drop(std::fs::remove_dir_all(&dir));
}

/// A decline ends the session: ensure_trust returns Err, and the path is
/// NOT persisted (no partial trust, no silent acceptance).
#[tokio::test]
async fn test_ensure_trust_declines() {
    use futures::SinkExt;
    use futures::StreamExt;
    use futures::channel::mpsc;
    use houyicoder_protocol::envelope::{
        ClientFrame, ClientResponseEnvelope, ClientResponsePayload, ServerFrame,
    };

    let dir = std::env::temp_dir().join(format!("trust-decline-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).unwrap();
    let settings = dir.join("settings.json");
    let proj = dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();

    let mut server = trust_prompt_server(settings.clone(), proj.clone());
    let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
    let mut io = super::super::frame_carrier::FrameCarrier::new(server_tx, server_rx);

    let feeder = tokio::spawn(async move {
        let mut ask_id = None;
        for _ in 0..16 {
            let line = client_rx.next().await.expect("server frame");
            if let Ok(ServerFrame::Request(req)) = serde_json::from_str(&line) {
                ask_id = Some(req.req_id);
                break;
            }
        }
        let ask_id = ask_id.expect("trust prompt sent");
        let resp = ClientFrame::Response(ClientResponseEnvelope::new(
            ask_id,
            ClientResponsePayload::TrustAccept(houyicoder_protocol::frontend::trust::TrustAccept {
                accepted: false,
            }),
        ));
        client_tx
            .send(encode_line(&resp))
            .await
            .expect("send decline");
    });

    let result = server.ensure_trust(&mut io).await;
    feeder.await.expect("feeder done");
    assert!(result.is_err(), "decline ends the session");
    assert!(
        !houyicoder_config::is_path_trusted(&settings, &proj),
        "decline must not persist trust"
    );

    drop(std::fs::remove_dir_all(&dir));
}

/// A client that closes while the trust prompt is in flight ends the
/// session with an Unavailable error: trust is never assumed from a
/// disconnect.
#[tokio::test]
async fn test_client_closed_during_prompt() {
    use futures::StreamExt;
    use futures::channel::mpsc;

    let dir = std::env::temp_dir().join(format!("trust-closed-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).unwrap();
    let settings = dir.join("settings.json");
    let proj = dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();

    let mut server = trust_prompt_server(settings, proj);
    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
    let mut io = super::super::frame_carrier::FrameCarrier::new(server_tx, server_rx);
    let task = tokio::spawn(async move { server.ensure_trust(&mut io).await });

    let _prompt = client_rx.next().await.expect("trust prompt frame");
    drop(client_tx);
    let err = task
        .await
        .expect("task join")
        .expect_err("a closed client ends the prompt");
    assert_eq!(err.category, ErrorCategory::Unavailable);
    assert_eq!(err.message, "client closed mid-trust-prompt");

    drop(std::fs::remove_dir_all(&dir));
}

/// A session/cancel while the trust prompt is in flight is a decline,
/// not a hang: the session ends with an Unavailable error naming the
/// cancel, and the project is not persisted as trusted.
#[tokio::test]
async fn test_cancel_declines_trust() {
    use futures::SinkExt;
    use futures::StreamExt;
    use futures::channel::mpsc;

    let dir = std::env::temp_dir().join(format!("trust-cancel-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).unwrap();
    let settings = dir.join("settings.json");
    let proj = dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();

    let mut server = trust_prompt_server(settings.clone(), proj.clone());
    let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
    let mut io = super::super::frame_carrier::FrameCarrier::new(server_tx, server_rx);
    let task = tokio::spawn(async move { server.ensure_trust(&mut io).await });

    let _prompt = client_rx.next().await.expect("trust prompt frame");
    let cancel = houyicoder_protocol::acp_wire::AcpNotification::new(
        "session/cancel",
        serde_json::json!({}),
    );
    client_tx
        .send(encode_line(&cancel))
        .await
        .expect("send cancel");
    let err = task
        .await
        .expect("task join")
        .expect_err("cancel declines the trust prompt");
    assert_eq!(err.category, ErrorCategory::Unavailable);
    assert_eq!(err.message, "user declined trust (cancel)");
    assert!(
        !houyicoder_config::is_path_trusted(&settings, &proj),
        "cancel must not persist trust"
    );

    drop(std::fs::remove_dir_all(&dir));
}

/// Answering the trust prompt with a permission payload fails closed:
/// the verdict shape is part of the contract, so the mismatch is an
/// InvalidFrame error rather than a guessed decline.
#[tokio::test]
async fn test_wrong_payload_rejected() {
    use futures::SinkExt;
    use futures::StreamExt;
    use futures::channel::mpsc;
    use houyicoder_protocol::envelope::{
        ClientFrame, ClientResponseEnvelope, ClientResponsePayload, ServerFrame,
    };
    use houyicoder_protocol::frontend::run::ApprovalDecision;

    let dir = std::env::temp_dir().join(format!("trust-payload-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).unwrap();
    let settings = dir.join("settings.json");
    let proj = dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();

    let mut server = trust_prompt_server(settings.clone(), proj.clone());
    let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
    let mut io = super::super::frame_carrier::FrameCarrier::new(server_tx, server_rx);
    let task = tokio::spawn(async move { server.ensure_trust(&mut io).await });

    let line = client_rx.next().await.expect("trust prompt frame");
    let ask_id = match serde_json::from_str(&line).expect("prompt frame parses") {
        ServerFrame::Request(req) => req.req_id,
        other => panic!("expected the trust prompt, got {other:?}"),
    };
    let resp = ClientFrame::Response(ClientResponseEnvelope::new(
        ask_id,
        ClientResponsePayload::Permission(ApprovalDecision {
            call_id: "c1".into(),
            approved: true,
            updated_input: None,
            scope: "once".to_string(),
        }),
    ));
    client_tx
        .send(encode_line(&resp))
        .await
        .expect("send the wrong payload");
    let err = task
        .await
        .expect("task join")
        .expect_err("a permission payload cannot answer the trust prompt");
    assert_eq!(err.category, ErrorCategory::InvalidFrame);
    assert_eq!(err.message, "expected a trust_accept reverse response");
    assert!(
        !houyicoder_config::is_path_trusted(&settings, &proj),
        "a rejected answer must not persist trust"
    );

    drop(std::fs::remove_dir_all(&dir));
}
