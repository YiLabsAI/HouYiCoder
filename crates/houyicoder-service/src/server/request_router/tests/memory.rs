//! Memory request dispatch tests.

#![cfg(test)]

use super::super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_config::recap::load_recap_from;
use houyicoder_context::SessionId;
use houyicoder_context::{MemoryEntry, MemoryError, MemoryScope, MemorySummary};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{MemoryGates, MemoryRuntime, Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::memory::MemoryToggleWhich;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;

use crate::server::recap::RecapSession;
use crate::server::recap::RecapSettings;
use std::env::temp_dir;
use std::fs::remove_dir_all;
use std::fs::remove_file;
use std::fs::write;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;

/// In-memory provider with a deterministic delete failure.
struct MockMemory;

impl MemoryProvider for MockMemory {
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
        Runner::new(
            store,
            Arc::new(FakeProvider::text("x")),
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
    let req = FrontendRequest::MemoryForget {
        key: key.into(),
        scope: "auto".into(),
    };
    memory_dispatch(None, vec![req])
        .await
        .pop()
        .expect("one reply")
}

/// Drive one server connection: send every request in order, then collect
/// the replies. settings None keeps the default path — only pass it for
/// requests that write settings, so tests never touch the real user file.
async fn memory_dispatch(
    settings: Option<PathBuf>,
    reqs: Vec<FrontendRequest>,
) -> Vec<ResponsePayload> {
    let runner = stub_runner_with_memory();
    let session = SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let recap_log: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let recap = Arc::new(RecapSession::new(
        session,
        RecapSettings::default(),
        Arc::new(FakeProvider::text("unused")),
        recap_log,
    ));
    let mut server =
        Server::new(runner, session, Arc::new(DefaultModeGate::new())).with_recap(recap);
    if let Some(path) = settings {
        server = server.with_settings_path(path);
    }
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    let count = reqs.len();
    for (i, payload) in reqs.into_iter().enumerate() {
        let req = ClientFrame::Request(RequestEnvelope::new(RequestId(i as u64 + 1), payload));
        send_line(&mut client_tx, &req);
    }
    let mut payloads = Vec::new();
    for _ in 0..count {
        match recv_frame(&mut client_rx).await {
            ServerFrame::Response(r) => payloads.push(r.payload),
            other => panic!("expected response, got {other:?}"),
        }
    }
    handle.abort();
    payloads
}

/// A show request replies with the MemoryShow payload for the key, so the
/// pane renders the entry on demand. The stub provider holds no entries, so
/// the detail is None, but the branch still maps the lookup to the
/// protocol form rather than dropping the request.
#[tokio::test]
async fn test_show_replies_memory_entry() {
    let payload = memory_dispatch(
        None,
        vec![FrontendRequest::MemoryShow { key: "any".into() }],
    )
    .await
    .pop()
    .expect("one reply");
    assert!(
        matches!(payload, ResponsePayload::MemoryShow(_)),
        "expected MemoryShow, got {payload:?}"
    );
}

/// A forget that succeeds replies with the refreshed (empty) MemoryList so
/// the pane narrows.
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
/// the delete worked).
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

/// A successful toggle persists to the settings file before the runtime
/// gate flips: the reply and a follow-up state read both show the new
/// value, and the settings file agrees, so the choice survives a restart.
#[tokio::test]
async fn test_toggle_persists_then_flips() {
    let dir = temp_dir().join(format!("toggle-ok-{}", id()));
    let settings = dir.join("settings.json");
    let payloads = memory_dispatch(
        Some(settings.clone()),
        vec![
            FrontendRequest::MemoryToggle {
                which: MemoryToggleWhich::Auto,
            },
            FrontendRequest::MemoryToggleState,
        ],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::ToggleState(state) => {
            assert!(!state.auto_memory, "reply carries the flipped pair");
            assert!(state.auto_dream, "the other toggle is untouched");
        }
        other => panic!("expected ToggleState, got {other:?}"),
    }
    match &payloads[1] {
        ResponsePayload::ToggleState(state) => {
            assert!(!state.auto_memory, "runtime gate flipped after persist");
        }
        other => panic!("expected ToggleState, got {other:?}"),
    }
    let (loaded, _w) = houyicoder_config::load_toggles_from(&settings);
    assert!(
        !loaded.auto_memory,
        "the flip is persisted, not just in-memory"
    );
    drop(remove_dir_all(&dir));
}

/// A settings write failure replies Error and leaves the runtime gate on
/// the old value — the pane never shows a toggle a restart would revert.
#[tokio::test]
async fn test_failed_toggle_keeps_state() {
    let blocker = temp_dir().join(format!("toggle-block-{}", id()));
    write(&blocker, "regular file, not a directory").unwrap();
    let payloads = memory_dispatch(
        Some(blocker.join("settings.json")),
        vec![
            FrontendRequest::MemoryToggle {
                which: MemoryToggleWhich::Auto,
            },
            FrontendRequest::MemoryToggleState,
        ],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::Error(e) => assert!(
            e.message.contains("failed to save settings"),
            "the write failure surfaces: {}",
            e.message
        ),
        other => panic!("expected Error, got {other:?}"),
    }
    match &payloads[1] {
        ResponsePayload::ToggleState(state) => {
            assert!(state.auto_memory, "runtime gate keeps the old value");
        }
        other => panic!("expected ToggleState, got {other:?}"),
    }
    drop(remove_file(&blocker));
}

/// The recap pair is written to the settings file and echoed back, so the
/// Config tab shows what a restart would read rather than what it hoped.
#[tokio::test]
async fn test_recap_config_persists() {
    let dir = temp_dir().join(format!("recap-ok-{}", id()));
    let settings = dir.join("settings.json");
    let payloads = memory_dispatch(
        Some(settings.clone()),
        vec![FrontendRequest::RecapConfigSet {
            enabled: true,
            idle_secs: 600,
        }],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::ToggleState(state) => {
            assert!(state.recap, "the reply carries the new switch");
            assert_eq!(state.recap_idle_secs, 600, "and the window it wrote");
        }
        other => panic!("expected ToggleState, got {other:?}"),
    }
    let (loaded, _w) = load_recap_from(&settings);
    assert!(loaded.enabled, "the switch is persisted");
    assert_eq!(loaded.idle_trigger_secs, 600, "so is the window");
    drop(remove_dir_all(&dir));
}

/// A switch turned on while the session runs applies to the live state: the
/// trigger the connection already attached to reads the new switch and window
/// without a restart, and the reply reports the same pair.
#[tokio::test]
async fn test_recap_config_applies_live() {
    let dir = temp_dir().join(format!("recap-live-{}", id()));
    let settings = dir.join("settings.json");
    let runner = stub_runner_with_memory();
    let session = SessionId::new();
    let log: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let recap = Arc::new(RecapSession::new(
        session,
        RecapSettings::default(),
        Arc::new(FakeProvider::text("unused")),
        log,
    ));
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let server = Server::new(runner, session, Arc::new(DefaultModeGate::new()))
        .with_recap(Arc::clone(&recap))
        .with_settings_path(settings);
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    let req = ClientFrame::Request(RequestEnvelope::new(
        RequestId(1),
        FrontendRequest::RecapConfigSet {
            enabled: true,
            idle_secs: 180,
        },
    ));
    send_line(&mut client_tx, &req);
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(response) => match response.payload {
            ResponsePayload::ToggleState(state) => {
                assert!(state.recap, "the reply carries the new switch");
                assert_eq!(state.recap_idle_secs, 180);
            }
            other => panic!("expected ToggleState, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
    let live = recap.settings();
    assert!(
        live.usable(),
        "the running session carries the switch the request wrote"
    );
    assert_eq!(live.idle_trigger_secs, 180);
    handle.abort();
    drop(remove_dir_all(&dir));
}

/// A write failure replies Error rather than a snapshot claiming the change
/// landed, so the tab never shows a switch a restart would revert.
#[tokio::test]
async fn test_recap_write_failure_reports() {
    let blocker = temp_dir().join(format!("recap-block-{}", id()));
    write(&blocker, "regular file, not a directory").unwrap();
    let payloads = memory_dispatch(
        Some(blocker.join("settings.json")),
        vec![FrontendRequest::RecapConfigSet {
            enabled: true,
            idle_secs: 300,
        }],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::Error(e) => {
            assert!(
                e.message.contains("settings"),
                "the refusal names the file: {e:?}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
