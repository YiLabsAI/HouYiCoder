//! Shared helpers for the service integration-test binaries.
//! Each tests-binary does mod common; + imports the frame plumbing so the
//! wire-contract tests share ONE copy of the stub-runner + channel pair +
//! send/recv helpers instead of duplicating them per binary. Cargo treats
//! tests/common/mod.rs as a helper module (NOT a separate test binary),
//! so it compiles into each binary that includes it.
//!
//! Not every helper is used by every binary (server_contract has its own
//! stub_runner with a specific canned reply; permission_wire uses the
//! generic one). A module-level dead-code allow keeps the unused-in-some-
//! binary helpers from gating the build.

#![allow(dead_code)] // test fixtures; used by some test binaries, unused from others

use std::sync::Arc;

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::ServerIo;
use houyicoder_session::SessionStore;

/// A minimal runner over a stub provider so a run completes in one turn (or
/// never drives the runner, for permission verbs that do not).
pub fn stub_runner() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let runner = Runner::with_shared_store(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    (Arc::new(runner), session)
}

/// The duplex channel pair a test client + the server share.
pub fn pair() -> (ServerIo, mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    (ServerIo::new(server_tx, server_rx), client_tx, client_rx)
}

/// Pull the next raw frame line the server sent, trailing newline stripped.
pub async fn recv_line(rx: &mut mpsc::Receiver<String>) -> String {
    let line = rx.next().await.expect("server sent a frame");
    line.strip_suffix('\n').unwrap_or(&line).to_string()
}

/// Decode the next server frame (post-handshake) as a typed ServerFrame.
pub async fn recv_frame(
    rx: &mut mpsc::Receiver<String>,
) -> houyicoder_protocol::envelope::ServerFrame {
    serde_json::from_str(&recv_line(rx).await).expect("frame decodes as a server frame")
}

/// recv_frame wrapped in a timeout so a drain loop that would hang (the
/// server never ships the expected frame, but the channel stays open — the
/// same shape as a session-mismatch hang) fails as a clear assertion
/// instead of a CI timeout. Panics with the hang cause on timeout.
pub async fn recv_frame_within(
    rx: &mut mpsc::Receiver<String>,
    dur: std::time::Duration,
) -> houyicoder_protocol::envelope::ServerFrame {
    match tokio::time::timeout(dur, recv_frame(rx)).await {
        Ok(frame) => frame,
        Err(_) => panic!(
            "timed out after {:?} waiting for a server frame; the server hung \
             (a concurrent-control frame was not handled — check session-id \
             match + the serve select! path)",
            dur
        ),
    }
}

/// Decode the next server frame as a typed Hello (handshake only).
pub async fn recv_hello(rx: &mut mpsc::Receiver<String>) -> Hello {
    serde_json::from_str(&recv_line(rx).await).expect("frame decodes as hello")
}

/// Send a typed frame from the client to the server, newline-terminated.
pub async fn send_frame(tx: &mut mpsc::Sender<String>, msg: &impl serde::Serialize) {
    let mut frame = encode(msg).expect("encode");
    if !frame.ends_with('\n') {
        frame.push('\n');
    }
    tx.send(frame).await.expect("client sends frame");
}
