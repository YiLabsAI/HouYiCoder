//! Tests for the ContextQuery dispatch handler. Covers the cache prefix,
//! hit rate, and compact buffer category injection in the /context reply.

#![cfg(test)]

use super::super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, Disposition, EventId, SessionEvent, SessionId,
    SessionLogEntry, TurnGroup,
};
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::ResponsePayload;
use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope, RequestId, ServerFrame};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::context::ContextBreakdown;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::sync::Arc;

fn stub_runner_with_checkpoint() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    // A cwd path that does not exist, so the measurement reads no project
    // memory file and the categories do not drift with the machine.
    let runner = Runner::new(
        store.clone(),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
    .with_cwd(temp_dir().join("houyi-context-dispatch-absent"));
    (Arc::new(runner), session)
}

/// Drive one /context request over a real server and return the reply. Frames
/// the server replays are drained by payload type, so the read does not depend
/// on how many frames precede the reply.
async fn context_breakdown(runner: Arc<Runner>, session: SessionId) -> ContextBreakdown {
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let server = Server::new(runner, session, Arc::new(DefaultModeGate::new()));
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    // The handshake frame is a bare Hello, not a ServerFrame.
    drop(client_rx.next().await);
    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(RequestId(1), FrontendRequest::Context)),
    );
    let breakdown = loop {
        if let ServerFrame::Response(reply) = recv_frame(&mut client_rx).await
            && let ResponsePayload::Context(breakdown) = reply.payload
        {
            break breakdown;
        }
    };
    handle.abort();
    breakdown
}

async fn seed_checkpoint(runner: &Runner, session: SessionId) {
    let event = SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::AssistantMessage {
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
    let bd = context_breakdown(runner, session).await;
    let has_compact = bd.categories.iter().any(|c| c.label == "Compact buffer");
    assert!(has_compact, "Compact buffer category present");
    assert!(
        bd.cache_prefix_tokens.is_some(),
        "cache prefix tokens populated"
    );
    // The prefix the request repeats is the whole system prompt, memory
    // blocks included, plus the tool schemas.
    let prompt = bd
        .categories
        .iter()
        .find(|c| c.label == "System prompt")
        .expect("system prompt category");
    let tools = bd
        .categories
        .iter()
        .find(|c| c.label == "System tools")
        .expect("tools category");
    assert_eq!(
        bd.cache_prefix_tokens,
        Some(prompt.tokens + tools.tokens),
        "the prefix covers the prompt and its tools"
    );
}

/// A session whose history is loaded but whose turn ran in another process
/// still reports that history: the measurement reads the store, so a resumed
/// session's /context is not a prompt-only view.
#[tokio::test]
async fn test_query_reports_loaded_history() {
    let (runner, session) = stub_runner_with_checkpoint();
    for event in [
        SessionEvent::UserInput {
            text: "how does the cache work".into(),
        },
        SessionEvent::AssistantMessage {
            text: "it reuses the prefix".into(),
            thinking: None,
        },
    ] {
        runner
            .store()
            .append(SessionLogEntry {
                id: EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                event,
            })
            .await
            .unwrap();
    }
    let bd = context_breakdown(runner, session).await;
    let labels: Vec<&str> = bd.categories.iter().map(|c| c.label.as_str()).collect();
    let messages = bd
        .categories
        .iter()
        .find(|c| c.label == "Messages")
        .expect("messages category");
    assert!(
        messages.tokens > 0,
        "the loaded history is measured: {labels:?}"
    );
    assert_eq!(
        labels.first_chunk::<4>(),
        Some(&["System prompt", "System tools", "Memory files", "Skills"]),
        "the categories keep their fixed order"
    );
    assert_eq!(
        bd.memory_files.len(),
        0,
        "a cwd without memory files reports no memory row"
    );
}
