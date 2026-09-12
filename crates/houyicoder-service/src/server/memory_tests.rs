//! Memory request dispatch tests.

#![cfg(test)]

use super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, MemoryError, MemoryScope, MemorySummary};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{MemoryGates, MemoryRuntime, Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::handshake::Hello;
use houyicoder_session::SessionStore;
use std::collections::HashSet;
use std::sync::Arc;

/// In-memory provider with a deterministic delete failure.
struct MockMemory;

impl MemoryProvider for MockMemory {
    fn recall(&self, _: &str, _: usize, _: &HashSet<String>) -> Vec<MemoryEntry> {
        Vec::new()
    }
    fn add(&self, _: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
    fn list_memories(&self) -> Vec<MemorySummary> {
        Vec::new()
    }
    fn delete_memory_in_scope(&self, key: &str, _scope: MemoryScope) -> Result<(), MemoryError> {
        if key == "fail" {
            Err(MemoryError::Io)
        } else {
            Ok(())
        }
    }
}

fn stub_runner_with_memory() -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runtime = MemoryRuntime::from_parts(
        store.clone(),
        Some(Arc::new(MockMemory)),
        MemoryGates::new(true, true),
        None,
        None,
    );
    Arc::new(
        Runner::with_shared_store(
            store,
            Arc::new(houyicoder_provider::FakeProvider::text("x")),
            ToolRegistry::new(),
            RunnerConfig {
                model: "test".into(),
                ..RunnerConfig::default()
            },
        )
        .install_memory(runtime),
    )
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

async fn forget_dispatch(key: &str) -> ResponsePayload {
    let runner = stub_runner_with_memory();
    let session = houyicoder_context::SessionId::new();
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
    let req = ClientFrame::Request(RequestEnvelope::new(
        RequestId(1),
        FrontendRequest::MemoryForget {
            key: key.into(),
            scope: "auto".into(),
        },
    ));
    send_line(&mut client_tx, &req);
    let payload = match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => r.payload,
        other => panic!("expected response, got {other:?}"),
    };
    handle.abort();
    payload
}

/// A forget that succeeds replies with the refreshed (empty) MemoryList so
/// the pane narrows. Pins the Ok arm of the dispatch match.
#[tokio::test]
async fn test_ok_replies_memory_list() {
    let payload = forget_dispatch("ok").await;
    match payload {
        ResponsePayload::MemoryList(entries) => {
            assert!(
                entries.is_empty(),
                "list narrowed after a successful forget"
            );
        }
        other => panic!("expected MemoryList, got {other:?}"),
    }
}

/// A forget that hits an Io failure replies with an Error (not a silent
/// MemoryList that would leave the entry present plus the user believing
/// the delete worked). Pins the Io-failure surfacing.
#[tokio::test]
async fn test_io_failure_replies_error() {
    let payload = forget_dispatch("fail").await;
    match payload {
        ResponsePayload::Error(e) => {
            assert!(
                e.message.contains("forget failed"),
                "Io failure surfaces to the user: {}",
                e.message
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
