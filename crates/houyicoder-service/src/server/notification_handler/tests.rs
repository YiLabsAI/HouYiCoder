//! Tests for in-flight turn control and session notifications.

use super::*;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnHandle, SpawnOutcome};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::sync::{Arc, Mutex};

fn server_no_runtime() -> Server {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("x"));
    let runner = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    Server::new(
        Arc::new(runner),
        SessionId::new(),
        Arc::new(DefaultModeGate::new()),
    )
}

/// Without a multi-agent runtime the handler logs the Err + drops (no
/// panic, no reply). The runner has no spawn handle, so steer_child
/// returns Err — the no-bus path on a single-agent server.
#[tokio::test]
async fn test_inject_child_no_runtime() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/inject_child",
        serde_json::json!({ "childSid": "c1", "text": "x" }),
    ));
}

/// A session/cancel_child_turn notification routes to the runner's
/// cancel_child. No spawn handle on a single-agent server, so the call
/// is a no-op (no panic).
#[tokio::test]
async fn test_cancel_child_routes() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/cancel_child_turn",
        serde_json::json!({ "childSid": "c1" }),
    ));
}

/// A session/cancel_child_turn with no childSid is a silent no-op.
#[tokio::test]
async fn test_cancel_child_no_sid() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/cancel_child_turn",
        serde_json::json!({}),
    ));
}

/// session/kill_child routes to the runner's lifecycle kill (terminal,
/// distinct from cancel_child_turn's per-turn). No runtime → no-op.
#[tokio::test]
async fn test_kill_child_routes() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/kill_child",
        serde_json::json!({ "childSid": "c1" }),
    ));
}

/// A session/kill_child with no childSid is a silent no-op.
#[tokio::test]
async fn test_kill_child_no_sid() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/kill_child",
        serde_json::json!({}),
    ));
}

/// A spawn port that records the children handed over to it, so the routing
/// assertion is about the call reaching the runtime rather than about the
/// handler merely not panicking.
struct RecordingSpawn {
    handed_over: Mutex<Vec<String>>,
}

impl SpawnHandle for RecordingSpawn {
    fn spawn(
        &self,
        _ctx: &ToolCtx,
        _args: SpawnArgs,
    ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
        Box::pin(async { Err(SpawnFailure::CapabilityDenied) })
    }

    fn background_child(&self, child_id: &str) -> bool {
        self.handed_over
            .lock()
            .expect("the record locks")
            .push(child_id.to_string());
        true
    }
}

/// session/background_child reaches the runtime's hand-over, named by the id
/// the reader selected. A handler that dropped the request would leave the
/// turn blocked with nothing on the wire to explain why.
#[tokio::test]
async fn test_background_child_routes() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("x"));
    let spawn = Arc::new(RecordingSpawn {
        handed_over: Mutex::new(Vec::new()),
    });
    let runner = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
    .with_spawn_handle(spawn.clone());
    let server = Server::new(
        Arc::new(runner),
        SessionId::new(),
        Arc::new(DefaultModeGate::new()),
    );

    server.handle_session_notification(&AcpNotification::new(
        "session/background_child",
        serde_json::json!({ "childSid": "c1" }),
    ));

    assert_eq!(
        spawn
            .handed_over
            .lock()
            .expect("the record locks")
            .as_slice(),
        ["c1".to_string()],
        "the handler hands over exactly the child the reader named"
    );
}

/// A session/background_child with no childSid is a silent no-op: it must not
/// hand over a child it cannot name.
#[tokio::test]
async fn test_background_child_no_sid() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/background_child",
        serde_json::json!({}),
    ));
}

/// session/kill_all routes to the runner's kill_all_children. With no
/// runtime attached it is a silent no-op (returns 0).
#[tokio::test]
async fn test_kill_all_routes() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/kill_all",
        serde_json::json!({}),
    ));
}

#[tokio::test]
async fn test_session_control_notifications() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/cancel",
        serde_json::json!({}),
    ));
    server.handle_session_notification(&AcpNotification::new(
        "session/inject",
        serde_json::json!({ "text": "follow up" }),
    ));
    server.handle_session_notification(&AcpNotification::new(
        "session/queue_remove",
        serde_json::json!({ "text": "follow up" }),
    ));
    server.handle_session_notification(&AcpNotification::new(
        "session/queue_remove",
        serde_json::json!({}),
    ));
    server.handle_session_notification(&AcpNotification::new(
        "session/inject_child",
        serde_json::json!({}),
    ));
    server.handle_session_notification(&AcpNotification::new(
        "session/inject_child",
        serde_json::json!({ "childSid": "c1", "text": "" }),
    ));
    server.handle_session_notification(&AcpNotification::new(
        "session/unknown",
        serde_json::json!({}),
    ));
}
