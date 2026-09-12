//! Memory command integration tests.

use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{MemoryGates, MemoryRuntime, Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::memory::MemoryToggleWhich;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::Server;
use houyicoder_session::SessionStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

mod common;
use common::{pair, recv_frame, recv_hello, send_frame};

fn stub_runner() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("hello"));
    let runner = Runner::with_shared_store(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    (Arc::new(runner), session)
}

/// Build a runner with one stored memory.
fn runner_with_memory(key: &str, body: &str) -> (Arc<Runner>, SessionId, std::path::PathBuf) {
    use houyicoder_context::{MemoryEntry, MemorySource};
    use houyicoder_memory::MarkdownMemoryProvider;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("houyi-forget-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create temp root");
    let memory: Arc<dyn houyicoder_api::memory::MemoryProvider> =
        Arc::new(MarkdownMemoryProvider::new(root.clone()));
    memory
        .add(MemoryEntry::new(key, body, MemorySource::Project))
        .expect("seed memory");
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("hello"));
    let runtime = MemoryRuntime::from_parts(
        store.clone(),
        Some(memory),
        MemoryGates::new(true, true),
        None,
        None,
    );
    let runner = Runner::with_shared_store(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    )
    .install_memory(runtime);
    (Arc::new(runner), session, root)
}

/// Return an isolated settings path.
fn temp_settings_path() -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("houyi-toggle-test-{}-{n}.json", std::process::id()))
}

/// Receive the requested toggle state.
async fn recv_toggle(
    resp_id: RequestId,
    rx: &mut futures::channel::mpsc::Receiver<String>,
) -> ResponsePayload {
    for _ in 0..16 {
        if let ServerFrame::Response(resp) = recv_frame(rx).await {
            assert_eq!(resp.req_id, resp_id, "response pairs to the request");
            return resp.payload;
        }
    }
    panic!("no ToggleState response for {resp_id:?}");
}

async fn handshake(
    tx: &mut futures::channel::mpsc::Sender<String>,
    rx: &mut futures::channel::mpsc::Receiver<String>,
) {
    send_frame(tx, &Hello::local()).await;
    let _ = recv_hello(rx).await;
}

#[tokio::test]
async fn test_toggle_read_flip_roundtrip() {
    let (runner, session) = stub_runner();
    let settings = temp_settings_path();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner.clone(),
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_settings_path(settings.clone());
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    handshake(&mut client_tx, &mut client_rx).await;

    let read_req = RequestEnvelope::new(RequestId(1), FrontendRequest::MemoryToggleState);
    send_frame(&mut client_tx, &ClientFrame::Request(read_req)).await;
    match recv_toggle(RequestId(1), &mut client_rx).await {
        ResponsePayload::ToggleState(state) => {
            assert!(state.auto_memory, "auto-memory defaults on");
            assert!(state.auto_dream, "auto-dream defaults on");
        }
        other => panic!("expected ToggleState, got {other:?}"),
    }

    let flip_req = RequestEnvelope::new(
        RequestId(2),
        FrontendRequest::MemoryToggle {
            which: MemoryToggleWhich::Auto,
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(flip_req)).await;
    match recv_toggle(RequestId(2), &mut client_rx).await {
        ResponsePayload::ToggleState(state) => {
            assert!(!state.auto_memory, "auto-memory flipped off");
            assert!(state.auto_dream, "auto-dream untouched");
        }
        other => panic!("expected ToggleState after flip, got {other:?}"),
    }

    let state = runner.memory_gate_state();
    assert!(!state.auto_memory, "runner atomic reflects the flip");
    assert!(state.auto_dream, "dream atomic untouched");
    let (persisted, _w) = houyicoder_config::load_toggles_from(&settings);
    assert!(!persisted.auto_memory, "persisted auto-memory off");
    assert!(persisted.auto_dream, "persisted auto-dream on");

    drop(client_tx);
    drop(handle.await);
    drop(std::fs::remove_file(&settings));
}

#[tokio::test]
async fn test_dream_leaves_auto_memory() {
    let (runner, session) = stub_runner();
    let settings = temp_settings_path();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_settings_path(settings.clone());
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    handshake(&mut client_tx, &mut client_rx).await;

    let flip_req = RequestEnvelope::new(
        RequestId(1),
        FrontendRequest::MemoryToggle {
            which: MemoryToggleWhich::Dream,
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(flip_req)).await;
    match recv_toggle(RequestId(1), &mut client_rx).await {
        ResponsePayload::ToggleState(state) => {
            assert!(state.auto_memory, "auto-memory untouched");
            assert!(!state.auto_dream, "auto-dream flipped off");
        }
        other => panic!("expected ToggleState, got {other:?}"),
    }

    drop(client_tx);
    drop(handle.await);
    drop(std::fs::remove_file(&settings));
}

/// Receive the requested memory list.
async fn recv_list(
    resp_id: RequestId,
    rx: &mut futures::channel::mpsc::Receiver<String>,
) -> Vec<houyicoder_protocol::frontend::memory::MemorySummaryEntry> {
    for _ in 0..16 {
        let frame = recv_frame(rx).await;
        let ServerFrame::Response(resp) = frame else {
            continue;
        };
        if resp.req_id != resp_id {
            continue;
        }
        let ResponsePayload::MemoryList(entries) = resp.payload else {
            continue;
        };
        return entries;
    }
    panic!("no MemoryList response for {resp_id:?}");
}

#[tokio::test]
async fn test_forget_archives_and_refreshes() {
    let (runner, session, root) = runner_with_memory("build-gate", "make check green");
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner.clone(),
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_settings_path(temp_settings_path());
    let handle = tokio::spawn(async move { server.serve(server_io).await });
    handshake(&mut client_tx, &mut client_rx).await;

    let list_req = RequestEnvelope::new(RequestId(1), FrontendRequest::MemoryList);
    send_frame(&mut client_tx, &ClientFrame::Request(list_req)).await;
    let before = recv_list(RequestId(1), &mut client_rx).await;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].key, "build-gate");

    let forget_req = RequestEnvelope::new(
        RequestId(2),
        FrontendRequest::MemoryForget {
            key: "build-gate".into(),
            scope: "auto".into(),
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(forget_req)).await;
    let after = recv_list(RequestId(2), &mut client_rx).await;
    assert!(after.is_empty(), "entry gone after forget");
    assert!(
        runner.memory_list().is_empty(),
        "runner list reflects delete"
    );

    drop(client_tx);
    drop(handle.await);
    drop(std::fs::remove_dir_all(&root));
}

/// Receive the requested error message.
#[cfg(unix)]
async fn recv_error(
    resp_id: RequestId,
    rx: &mut futures::channel::mpsc::Receiver<String>,
) -> String {
    for _ in 0..16 {
        let frame = recv_frame(rx).await;
        let ServerFrame::Response(resp) = frame else {
            continue;
        };
        if resp.req_id != resp_id {
            continue;
        }
        let ResponsePayload::Error(e) = resp.payload else {
            continue;
        };
        return e.message;
    }
    panic!("no Error response for {resp_id:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn test_forget_surfaces_io_failure() {
    use std::os::unix::fs::PermissionsExt;
    let (runner, session, root) = runner_with_memory("io-gate", "make check green");
    // Make the root read-only so the topic delete fails with Io (deleting a
    // file needs write permission on the directory, not the file).
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).expect("set read-only");
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner.clone(),
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_settings_path(temp_settings_path());
    let handle = tokio::spawn(async move { server.serve(server_io).await });
    handshake(&mut client_tx, &mut client_rx).await;
    let forget_req = RequestEnvelope::new(
        RequestId(1),
        FrontendRequest::MemoryForget {
            key: "io-gate".into(),
            scope: "auto".into(),
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(forget_req)).await;
    let msg = recv_error(RequestId(1), &mut client_rx).await;
    assert!(
        msg.contains("forget failed"),
        "Io failure surfaces to the user: {msg}"
    );
    drop(client_tx);
    drop(handle.await);
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).ok();
    drop(std::fs::remove_dir_all(&root));
}
