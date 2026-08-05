//! MemoryForget dispatch handler — inline (src/) tests. The handler is
//! async + needs ServerIo, testable inline via ServerIo::new (mirroring
//! rename_session_tests), so these count toward --lib diff-cov (make check's
//! --lib lcov cannot see tests/ coverage). Covers the Ok -> MemoryList path
//! and the Io-failure -> Error surfacing.

#![cfg(test)]

use super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, MemoryError, MemoryScope, MemorySummary};
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

/// A memory provider that fails the delete for the key "fail" (Io) and
/// succeeds for every other key. Lets the dispatch handler exercise both the
/// MemoryList reply (Ok) and the Error reply (Io) without a filesystem.
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

fn stub_runner_with_memory() -> Arc<houyicoder_core::agent::Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(
        houyicoder_core::agent::Runner::with_shared_store(
            store,
            Arc::new(houyicoder_provider::FakeProvider::text("x")),
            houyicoder_core::agent::ToolRegistry::new(),
            houyicoder_core::agent::runner_config::RunnerConfig {
                model: "test".into(),
                ..houyicoder_core::agent::runner_config::RunnerConfig::default()
            },
        )
        .with_memory(Arc::new(MockMemory)),
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
