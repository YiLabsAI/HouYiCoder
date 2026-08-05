//! /debug dispatch handler tests, inline (src/) so they count toward --lib
//! diff-cov. Mirrors the model_set_tests precedent: the handler is async +
//! needs ServerIo, which is testable inline via ServerIo::new.

#![cfg(test)]

use super::super::*;
use crate::diagnostics;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::handshake::Hello;
use houyicoder_session::SessionStore;
use std::sync::Arc;

fn stub_runner() -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(Runner::with_shared_store(
        store,
        Arc::new(houyicoder_provider::FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ))
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

/// A /debug request against a server with no sink installed replies with an
/// error naming the absence, not a silent success.
#[tokio::test]
async fn test_no_sink_replies_error() {
    let runner = stub_runner();
    let session = SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = ServerIo::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_diagnostics(None);
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(1),
            FrontendRequest::DebugSet {
                level: houyicoder_protocol::frontend::debug::DebugLevel::Debug,
            },
        )),
    );
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::Error(e) => {
                assert!(
                    e.message.contains("no diagnostic sink"),
                    "expected the absence named, got {e:?}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
    handle.abort();
}

/// A /debug request against a server WITH a sink installed sets the level
/// and replies with the enabled state + the file path. Uses the
/// process-wide handle that install() stored (install_claims test claims
/// it for the process), so the server's set_level targets the real
/// subscriber.
#[tokio::test]
async fn test_set_level_replies_enabled() {
    // install_claims_and_rejects_second must have run to install the
    // process-wide sink. If it has not (test ordering), skip: the
    // integration test covers this path end-to-end.
    let diag_handle = match diagnostics::handle() {
        Some(h) => h,
        None => {
            eprintln!("debug_set_level_replies_enabled: no global sink, skipping");
            return;
        }
    };
    let runner = stub_runner();
    let session = SessionId::new();

    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = ServerIo::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_diagnostics(Some(diag_handle));
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(1),
            FrontendRequest::DebugSet {
                level: houyicoder_protocol::frontend::debug::DebugLevel::Debug,
            },
        )),
    );
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::Debug(s) => {
                assert!(s.enabled, "enabled after Debug set");
            }
            other => panic!("expected Debug state, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }

    // Turn it back off so the filter is clean for the next test.
    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(2),
            FrontendRequest::DebugSet {
                level: houyicoder_protocol::frontend::debug::DebugLevel::Off,
            },
        )),
    );
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::Debug(s) => assert!(!s.enabled, "disabled after Off set"),
            other => panic!("expected Debug state, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
    handle.abort();
}
