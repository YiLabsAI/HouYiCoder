//! Tests for the RenameSession dispatch handler. Covers the happy path,
//! empty-name-clears-to-auto, the three fail-closed branches, and a write
//! failure.

#![cfg(test)]

use super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_context::{
    NameSource, SessionDescriptor, SessionDescriptorStore, SessionProvenance,
};
use houyicoder_memory::{InMemoryBackend, InMemoryDescriptorStore};
use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope, RequestId, ServerFrame};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::handshake::Hello;
use houyicoder_session::SessionStore;
use std::sync::Arc;

fn stub_runner() -> Arc<houyicoder_core::agent::Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(houyicoder_core::agent::Runner::with_shared_store(
        store,
        Arc::new(houyicoder_provider::FakeProvider::text("x")),
        houyicoder_core::agent::ToolRegistry::new(),
        houyicoder_core::agent::runner_config::RunnerConfig {
            model: "test".into(),
            ..houyicoder_core::agent::runner_config::RunnerConfig::default()
        },
    ))
}

fn seeded_store(session: houyicoder_context::SessionId) -> Arc<dyn SessionDescriptorStore> {
    let store: Arc<dyn SessionDescriptorStore> = Arc::new(InMemoryDescriptorStore::new());
    store
        .write_descriptor(
            session,
            &SessionDescriptor {
                name: None,
                name_source: NameSource::Auto,
                cwd: "/tmp".into(),
                model: "test".into(),
                provenance: SessionProvenance::Fresh,
                version: "test".into(),
                created_at: 1000,
                child_session_ids: Vec::new(),
            },
        )
        .unwrap();
    store
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

/// A non-empty name persists name + name_source=User + replies with a Status
/// snapshot carrying the new name.
#[tokio::test]
async fn test_rename_sets_user_name() {
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let wire_sid = houyicoder_protocol::frontend::SessionId(session.to_string());
    let store = seeded_store(session);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_descriptor_store(store.clone());
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(1),
            FrontendRequest::RenameSession {
                session_id: wire_sid,
                name: "fix-login".into(),
            },
        )),
    );
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            houyicoder_protocol::envelope::ResponsePayload::Status(snap) => {
                let name = snap.descriptor.as_ref().and_then(|m| m.name.as_deref());
                assert_eq!(name, Some("fix-login"), "reply carries the new name");
            }
            other => panic!("expected Status, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
    let after = store.read_descriptor(session).expect("sidecar persisted");
    assert_eq!(after.name.as_deref(), Some("fix-login"));
    assert_eq!(after.name_source, NameSource::User, "marked User source");
    handle.abort();
}

/// An empty/whitespace name clears back to Auto (name=None).
#[tokio::test]
async fn test_empty_clears_to_auto() {
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let wire_sid = houyicoder_protocol::frontend::SessionId(session.to_string());
    let store = seeded_store(session);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_descriptor_store(store.clone());
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(2),
            FrontendRequest::RenameSession {
                session_id: wire_sid,
                name: "   ".into(),
            },
        )),
    );
    let resp = recv_frame(&mut client_rx).await;
    assert!(matches!(
        resp,
        ServerFrame::Response(r) if matches!(r.payload, houyicoder_protocol::envelope::ResponsePayload::Status(_))
    ));
    let after = store.read_descriptor(session).expect("sidecar persisted");
    assert!(after.name.is_none(), "empty name clears to None");
    assert_eq!(after.name_source, NameSource::Auto, "marked Auto source");
    handle.abort();
}

/// A mismatched session id fails closed with an InvalidRequest error.
#[tokio::test]
async fn test_rename_mismatch_sid_errors() {
    use houyicoder_protocol::error::ErrorCategory;
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let store = seeded_store(session);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_descriptor_store(store);
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(3),
            FrontendRequest::RenameSession {
                session_id: houyicoder_protocol::frontend::SessionId("wrong-sid".into()),
                name: "x".into(),
            },
        )),
    );
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            houyicoder_protocol::envelope::ResponsePayload::Error(e) => {
                assert_eq!(
                    e.category,
                    ErrorCategory::InvalidRequest,
                    "mismatch fails closed"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
    handle.abort();
}

/// A missing descriptor store returns an Internal error.
#[tokio::test]
async fn test_no_descriptor_store_errors() {
    use houyicoder_protocol::error::ErrorCategory;
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let wire_sid = houyicoder_protocol::frontend::SessionId(session.to_string());
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(4),
            FrontendRequest::RenameSession {
                session_id: wire_sid,
                name: "x".into(),
            },
        )),
    );
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            houyicoder_protocol::envelope::ResponsePayload::Error(e) => {
                assert_eq!(
                    e.category,
                    ErrorCategory::Internal,
                    "no-store errors (server lacks the store)"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
    handle.abort();
}

/// A wired store with no sidecar for the session errors Internal.
#[tokio::test]
async fn test_rename_no_sidecar_errors() {
    use houyicoder_protocol::error::ErrorCategory;
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let wire_sid = houyicoder_protocol::frontend::SessionId(session.to_string());
    let store: Arc<dyn SessionDescriptorStore> = Arc::new(InMemoryDescriptorStore::new());
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_descriptor_store(store);
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(5),
            FrontendRequest::RenameSession {
                session_id: wire_sid,
                name: "x".into(),
            },
        )),
    );
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            houyicoder_protocol::envelope::ResponsePayload::Error(e) => {
                assert_eq!(
                    e.category,
                    ErrorCategory::Internal,
                    "no-sidecar errors (server has no sidecar)"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
    handle.abort();
}

struct FailingDescriptorStore(InMemoryDescriptorStore);

impl SessionDescriptorStore for FailingDescriptorStore {
    fn read_descriptor(&self, session: houyicoder_context::SessionId) -> Option<SessionDescriptor> {
        self.0.read_descriptor(session)
    }
    fn write_descriptor(
        &self,
        _session: houyicoder_context::SessionId,
        _descriptor: &SessionDescriptor,
    ) -> Result<(), houyicoder_context::SessionDescriptorError> {
        Err(houyicoder_context::SessionDescriptorError(
            "simulated write failure".into(),
        ))
    }
    fn update_descriptor(
        &self,
        _session: houyicoder_context::SessionId,
        _edit: &mut dyn FnMut(&mut SessionDescriptor),
    ) -> Result<houyicoder_context::DescriptorUpdate, houyicoder_context::SessionDescriptorError>
    {
        // Fails like write_descriptor: the edit is never applied, so the rename
        // path still sees a write failure rather than a silent success.
        Err(houyicoder_context::SessionDescriptorError(
            "simulated write failure".into(),
        ))
    }
    fn delete_descriptor(&self, session: houyicoder_context::SessionId) {
        self.0.delete_descriptor(session);
    }
    fn list_descriptors(&self) -> Vec<(houyicoder_context::SessionId, SessionDescriptor)> {
        self.0.list_descriptors()
    }
}

/// A sidecar write failure surfaces as an Internal error.
#[tokio::test]
async fn test_rename_write_failure_errors() {
    use houyicoder_protocol::error::ErrorCategory;
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let wire_sid = houyicoder_protocol::frontend::SessionId(session.to_string());
    let inner = InMemoryDescriptorStore::new();
    inner
        .write_descriptor(
            session,
            &SessionDescriptor {
                name: None,
                name_source: NameSource::Auto,
                cwd: "/tmp".into(),
                model: "test".into(),
                provenance: SessionProvenance::Fresh,
                version: "test".into(),
                created_at: 1000,
                child_session_ids: Vec::new(),
            },
        )
        .unwrap();
    let store: Arc<dyn SessionDescriptorStore> = Arc::new(FailingDescriptorStore(inner));
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_descriptor_store(store);
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(6),
            FrontendRequest::RenameSession {
                session_id: wire_sid,
                name: "x".into(),
            },
        )),
    );
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            houyicoder_protocol::envelope::ResponsePayload::Error(e) => {
                assert_eq!(
                    e.category,
                    ErrorCategory::Internal,
                    "write failure => Internal"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
    handle.abort();
}
