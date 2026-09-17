//! Tests for the ModelSet dispatch handler and the ModelInfo projection:
//! the id swap, the Default sentinel, effort pass-through to the reply, the
//! persistence outcome, and the catalog rows the pane renders.

#![cfg(test)]

use super::*;
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
use houyicoder_protocol::frontend::model::{
    EffectiveFrom, ModelChoice, PersistenceOutcome, SpeedMode,
};
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::llm::EffortLevel;
use houyicoder_session::SessionStore;

use crate::composition::catalog_resolver::SettingsCatalogResolver;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, fs, process};

/// The runner the fixture server runs on: the stub provider plus a catalog
/// resolver over the fixture's own settings file. The capabilities the server
/// reports for a row then come from the rows the server itself read, not from
/// the global settings path, which a test must not touch.
fn runner_with_settings(path: &Path) -> Arc<Runner> {
    let (section, _warnings) = houyicoder_config::load_model_section_from(path);
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(
        Runner::new(
            store,
            Arc::new(houyicoder_provider::FakeProvider::text("x")),
            ToolRegistry::new(),
            RunnerConfig {
                model: "stub-model".into(),
                ..RunnerConfig::default()
            },
        )
        .with_catalog_resolver(Arc::new(SettingsCatalogResolver::from_section(section))),
    )
}

/// A unique temp settings path so a ModelSet's persist_model_pick writes the
/// temp file (not the developer's real HOME settings) + the test stays
/// isolated from other crates' settings reads.
fn temp_settings(slug: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!("model-set-{slug}-{n}-{}.json", process::id()))
}

fn send_line(tx: &mut mpsc::Sender<String>, frame: &impl serde::Serialize) {
    let mut s = houyicoder_protocol::framing::encode(frame).unwrap();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    tx.try_send(s).unwrap();
}

/// A live server over an in-process carrier, with the settings file written
/// before the server starts so the resolution chain reads it.
struct Fixture {
    runner: Arc<Runner>,
    tx: mpsc::Sender<String>,
    rx: mpsc::Receiver<String>,
    handle: tokio::task::JoinHandle<Result<(), ProtocolError>>,
}

impl Fixture {
    async fn start(settings_json: &str) -> Self {
        let path = temp_settings("fixture");
        fs::write(&path, settings_json).unwrap();
        Self::start_at(path).await
    }

    async fn start_at(path: PathBuf) -> Self {
        let runner = runner_with_settings(&path);
        let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
        let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
        let server = Server::new(
            runner.clone(),
            SessionId::new(),
            Arc::new(houyicoder_permission::DefaultModeGate::new()),
        )
        .with_settings_path(path);
        let handle =
            tokio::spawn(
                async move { server.serve(FrameCarrier::new(server_tx, server_rx)).await },
            );
        send_line(&mut client_tx, &Hello::local());
        drop(client_rx.next().await); // server Hello
        Self {
            runner,
            tx: client_tx,
            rx: client_rx,
            handle,
        }
    }

    async fn request(&mut self, req_id: u64, payload: FrontendRequest) -> ResponsePayload {
        send_line(
            &mut self.tx,
            &ClientFrame::Request(RequestEnvelope::new(RequestId(req_id), payload)),
        );
        for _ in 0..32 {
            let line = self.rx.next().await.unwrap();
            let Ok(frame) = serde_json::from_str::<ServerFrame>(line.trim_end()) else {
                continue;
            };
            let ServerFrame::Response(envelope) = frame else {
                continue;
            };
            if envelope.req_id == RequestId(req_id) {
                return envelope.payload;
            }
        }
        panic!("no response for request {req_id}");
    }

    fn close(self) {
        self.handle.abort();
    }
}

/// A Some(model) ModelSet swaps the runner's active model + replies with the
/// pick, the applied model and the effective-from marker.
#[tokio::test]
async fn test_model_set_some_swaps() {
    let mut fx = Fixture::start("{}").await;
    let payload = fx
        .request(
            1,
            FrontendRequest::ModelSet {
                model: Some("glm-5.2".into()),
                effort: None,
                effort_toggled: false,
                speed: None,
            },
        )
        .await;

    let ResponsePayload::ModelResult(result) = payload else {
        panic!("expected ModelResult, got {payload:?}");
    };
    assert_eq!(result.applied.id, "glm-5.2", "reply carries the applied id");
    assert_eq!(result.applied.speed, SpeedMode::Standard, "no pick = off");
    assert_eq!(result.effective_from, EffectiveFrom::Immediate);
    assert_eq!(result.persistence, PersistenceOutcome::Saved);
    assert_eq!(fx.runner.active_model(), "glm-5.2", "runner model switched");
    fx.close();
}

/// A settings write that cannot land is reported as itself: the session did
/// switch, and the reply names the destination that failed instead of
/// merging it into a vague loss.
#[cfg(unix)]
#[tokio::test]
async fn test_set_reports_settings_loss() {
    use std::os::unix::fs::PermissionsExt;
    let dir = env::temp_dir().join(format!("model-set-locked-{}", process::id()));
    // Clear a read-only dir an earlier failed run may have left behind.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).ok();
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("settings.json");
    fs::write(&path, "{}").unwrap();
    // A read-only dir blocks the lock and temp files the settings write needs.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let mut fx = Fixture::start_at(path).await;
    let payload = fx
        .request(
            1,
            FrontendRequest::ModelSet {
                model: Some("glm-5.2".into()),
                effort: None,
                effort_toggled: false,
                speed: None,
            },
        )
        .await;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let ResponsePayload::ModelResult(result) = payload else {
        panic!("expected ModelResult, got {payload:?}");
    };
    assert_eq!(result.applied.id, "glm-5.2", "the session still switched");
    match result.persistence {
        PersistenceOutcome::Partial { settings, .. } => assert!(
            settings.as_deref().is_some_and(|e| !e.is_empty()),
            "the failed destination carries its own loss"
        ),
        other => panic!("expected Partial, got {other:?}"),
    }
    fx.close();
    fs::remove_dir_all(&dir).ok();
}

/// A None model ModelSet applies the built-in default, not the settings
/// model.id it replaces: that id is the pick being declined. The session
/// effort comes from the request and the reply carries the resolved level.
#[tokio::test]
async fn test_set_none_resolves_sentinel() {
    let mut fx = Fixture::start(r#"{"model":{"id":"glm-5.2"}}"#).await;
    let payload = fx
        .request(
            2,
            FrontendRequest::ModelSet {
                model: None,
                effort: Some(EffortLevel::High),
                effort_toggled: true,
                speed: None,
            },
        )
        .await;

    let ResponsePayload::ModelResult(result) = payload else {
        panic!("expected ModelResult, got {payload:?}");
    };
    assert_eq!(
        result.applied.id,
        houyicoder_config::DEFAULT_MODEL,
        "Default applies the constant, not the settings id"
    );
    assert_eq!(
        result.applied.effort,
        Some(EffortLevel::High),
        "qwen3 speaks the dialect, so the session effort is honored"
    );
    assert_eq!(
        fx.runner.active_model(),
        houyicoder_config::DEFAULT_MODEL,
        "runner swapped to the default"
    );
    fx.close();
}

/// A Some model with an effort the model supports echoes the applied effort
/// back (qwen3 speaks the qwen3 dialect, so High is honored).
#[tokio::test]
async fn test_model_set_effort_applied() {
    let mut fx = Fixture::start("{}").await;
    let payload = fx
        .request(
            3,
            FrontendRequest::ModelSet {
                model: Some("qwen3.7-max".into()),
                effort: Some(EffortLevel::High),
                effort_toggled: true,
                speed: None,
            },
        )
        .await;

    let ResponsePayload::ModelResult(result) = payload else {
        panic!("expected ModelResult, got {payload:?}");
    };
    assert_eq!(result.applied.id, "qwen3.7-max");
    assert_eq!(
        result.applied.effort,
        Some(EffortLevel::High),
        "qwen3 supports effort"
    );
    fx.close();
}

/// A pick above the levels the model accepts clamps once, everywhere: the
/// session state, the reply and the persisted settings all carry the level
/// that will actually run, so the pane never shows a level the host would
/// clamp on the next request.
#[tokio::test]
async fn test_set_clamps_effort_persist() {
    let path = temp_settings("clamp");
    fs::write(&path, r#"{"model":{"catalog":[{"id":"qwen3.7-max"}]}}"#).unwrap();
    let mut fx = Fixture::start_at(path.clone()).await;
    let payload = fx
        .request(
            3,
            FrontendRequest::ModelSet {
                model: Some("qwen3.7-max".into()),
                effort: Some(EffortLevel::XHigh),
                effort_toggled: true,
                speed: None,
            },
        )
        .await;
    let ResponsePayload::ModelResult(result) = payload else {
        panic!("expected ModelResult, got {payload:?}");
    };
    assert_eq!(
        result.applied.effort,
        Some(EffortLevel::High),
        "the reply reports the level that runs"
    );
    assert_eq!(
        fx.runner.resolve_applied_effort(),
        Some(EffortLevel::High),
        "the session runs the clamped level"
    );
    let back: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        back["model"]["catalog"][0]["effort"], "high",
        "what is saved is what runs, never the pre-clamp request: {back}"
    );
    fx.close();
    drop(fs::remove_file(&path));
}

/// A ModelInfo request projects the settings catalog into the pane snapshot
/// in written order, while the live session supplies the selection and the
/// applied model. A session model the catalog does not list gets a row of its
/// own, appended so the written order is untouched and the pane has something
/// to put the check and the cursor on.
#[tokio::test]
async fn test_info_projects_catalog_rows() {
    let mut fx = Fixture::start(
        r#"{"model":{"id":"qwen3.7-max","catalog":[{"id":"qwen3.7-max","display_name":"Max","description":"most capable"},{"id":"glm-5.2","display_name":"Fable"}]}}"#,
    )
    .await;
    let payload = fx.request(4, FrontendRequest::ModelInfo).await;

    let ResponsePayload::ModelInfo(catalog) = payload else {
        panic!("expected ModelInfo, got {payload:?}");
    };
    assert_eq!(
        catalog.applied.id, "stub-model",
        "the live session model, not the settings id"
    );
    assert_eq!(
        catalog.selected,
        ModelChoice::Explicit {
            id: "stub-model".into()
        },
        "the selection follows the live session model"
    );
    assert_eq!(
        catalog.entries.len(),
        3,
        "the two written rows plus the live session's"
    );
    assert_eq!(catalog.entries[0].id, "qwen3.7-max");
    assert_eq!(catalog.entries[0].display_name.as_deref(), Some("Max"));
    assert_eq!(catalog.entries[1].display_name.as_deref(), Some("Fable"));
    assert_eq!(
        catalog.entries[2].id, "stub-model",
        "the live session's row is appended, so the check has a row to land on"
    );
    assert_eq!(
        catalog.resolved_default.id,
        houyicoder_config::DEFAULT_MODEL
    );
    fx.close();
}

/// A Fast pick follows the target model: a row that declares a fast tier
/// keeps it, a row that does not drops to Standard rather than leaving the
/// session asking for a tier the model cannot serve.
#[tokio::test]
async fn test_set_fast_follows_target() {
    let mut plain = Fixture::start(r#"{"model":{"catalog":[{"id":"glm-5.2"}]}}"#).await;
    let payload = plain
        .request(
            6,
            FrontendRequest::ModelSet {
                model: Some("glm-5.2".into()),
                effort: None,
                effort_toggled: false,
                speed: Some(SpeedMode::Fast),
            },
        )
        .await;
    let ResponsePayload::ModelResult(result) = payload else {
        panic!("expected ModelResult, got {payload:?}");
    };
    assert_eq!(result.applied.id, "glm-5.2");
    assert_eq!(
        result.applied.speed,
        SpeedMode::Standard,
        "a model with no fast tier is not given one"
    );
    assert_eq!(
        plain.runner.active_speed(),
        SpeedMode::Standard,
        "the session itself drops to Standard, not only the reply"
    );
    plain.close();

    let mut served =
        Fixture::start(r#"{"model":{"catalog":[{"id":"glm-5.2","fast":true}]}}"#).await;
    let payload = served
        .request(
            7,
            FrontendRequest::ModelSet {
                model: Some("glm-5.2".into()),
                effort: None,
                effort_toggled: false,
                speed: Some(SpeedMode::Fast),
            },
        )
        .await;
    let ResponsePayload::ModelResult(result) = payload else {
        panic!("expected ModelResult, got {payload:?}");
    };
    assert_eq!(
        result.applied.speed,
        SpeedMode::Fast,
        "a tier the row declares is applied as picked"
    );
    assert_eq!(served.runner.active_speed(), SpeedMode::Fast);
    served.close();
}

/// A session model the catalog already lists is not appended a second time:
/// the pane shows one row per written entry and the check lands on the row the
/// settings already carry.
#[tokio::test]
async fn test_info_keeps_written_rows() {
    let mut fx = Fixture::start(
        r#"{"model":{"catalog":[{"id":"stub-model","display_name":"Stub"},{"id":"glm-5.2","display_name":"Fable"}]}}"#,
    )
    .await;
    let payload = fx.request(5, FrontendRequest::ModelInfo).await;

    let ResponsePayload::ModelInfo(catalog) = payload else {
        panic!("expected ModelInfo, got {payload:?}");
    };
    assert_eq!(
        catalog.entries.len(),
        2,
        "the live session model is already a row: no second copy"
    );
    assert_eq!(catalog.entries[0].id, "stub-model");
    assert_eq!(catalog.entries[1].id, "glm-5.2");
    assert_eq!(
        catalog.selected,
        ModelChoice::Explicit {
            id: "stub-model".into()
        },
        "the check lands on the written row"
    );
    fx.close();
}
