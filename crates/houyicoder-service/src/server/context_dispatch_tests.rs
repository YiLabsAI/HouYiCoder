//! ContextQuery dispatch handler — inline (src/) tests. Covers the cache
//! prefix + hit rate + compact buffer category injection in the /context
//! reply. Mirrors the rename_session_tests pattern so these count toward
//! --lib diff-cov.

#![cfg(test)]

use super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, Disposition, EventId, SessionId, TurnEvent, TurnEventKind,
    TurnGroup,
};
use houyicoder_core::agent::Runner;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope, RequestId, ServerFrame};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::sync::Arc;

fn stub_runner_with_checkpoint() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let runner = Runner::with_shared_store(
        store.clone(),
        Arc::new(FakeProvider::text("x")),
        houyicoder_core::agent::ToolRegistry::new(),
        houyicoder_core::agent::runner_config::RunnerConfig::default(),
    );
    (Arc::new(runner), session)
}

async fn seed_checkpoint(runner: &Runner, session: SessionId) {
    let event = TurnEvent {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        kind: TurnEventKind::AssistantMessage {
            text: "folded".into(),
            thinking: None,
        },
    };
    runner.store().append(event.clone()).await.unwrap();
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session,
        last_event: event.id,
        summary: Some("summary of folded turns".into()),
        plan: vec![TurnGroup {
            turn_id: event.id,
            disposition: Disposition::Summarized,
            event_ids: vec![event.id],
        }],
        ts: 0,
    };
    runner
        .store()
        .backend()
        .write_checkpoint(manifest)
        .await
        .unwrap();
}

fn send_line(tx: &mut mpsc::Sender<String>, frame: &impl serde::Serialize) {
    let mut s = encode(frame).unwrap();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    tx.try_send(s).unwrap();
}

async fn recv_frame(rx: &mut mpsc::Receiver<String>) -> ServerFrame {
    serde_json::from_str(&rx.next().await.unwrap()).expect("frame decodes")
}

#[tokio::test]
async fn test_query_injects_compact_buffer() {
    let (runner, session) = stub_runner_with_checkpoint();
    seed_checkpoint(&runner, session).await;
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = ServerIo::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    // Skip the seeded "folded" assistant message that the server replays.
    drop(client_rx.next().await);
    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(RequestId(1), FrontendRequest::Status)),
    );
    let _snap = match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            houyicoder_protocol::envelope::ResponsePayload::Status(s) => s,
            _ => panic!("expected Status"),
        },
        other => panic!("expected status response: {other:?}"),
    };
    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(RequestId(2), FrontendRequest::Context)),
    );
    let bd = match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            houyicoder_protocol::envelope::ResponsePayload::Context(bd) => bd,
            _ => panic!("expected Context"),
        },
        other => panic!("expected context response: {other:?}"),
    };
    let has_compact = bd.categories.iter().any(|c| c.label == "Compact buffer");
    assert!(has_compact, "Compact buffer category present");
    assert!(
        bd.cache_prefix_tokens.is_some(),
        "cache prefix tokens populated"
    );
    handle.abort();
}
