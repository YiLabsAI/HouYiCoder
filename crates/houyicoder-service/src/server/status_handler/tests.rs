//! Tests for the status snapshot's carried fields.

use super::*;
use crate::composition::BuildRunnerOptions;
use crate::composition::build_runner;
use houyicoder_context::{
    EventId, NameSource, PromptCoverage, PromptRecord, SessionDescriptor, SessionDescriptorStore,
    SessionEvent, SessionId, SessionLogEntry, SessionProvenance,
};
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::{InMemoryBackend, InMemoryDescriptorStore, LocalFileBackend};
use houyicoder_permission::DefaultModeGate;
use houyicoder_provider::FakeProvider;
use houyicoder_provider::ProviderFacts;
use houyicoder_provider::ProviderResolution;
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::process::id;
use std::sync::Arc;

/// The snapshot reports the endpoint the provider was built from, not a fresh
/// environment resolution. The two disagree whenever the settings file pins an
/// endpoint, which is the case that used to make this field name a host the
/// session never contacted.
#[test]
fn test_snapshot_reports_carried_endpoint() {
    let root = temp_dir().join(format!("status-endpoint-{}-{}", id(), line!()));
    drop(remove_dir_all(&root));
    create_dir_all(&root).expect("mkdir root");
    let options = BuildRunnerOptions {
        project: Some(root.to_string_lossy().into_owned()),
        provider: Some(ProviderResolution::stub()),
        ..Default::default()
    };
    let bundle = build_runner(options);
    let carried = ProviderFacts {
        base_url: "https://team-gateway.example/compatible-mode/v1".into(),
        key_origin: Some("keychain houyicoder/dashscope-team".into()),
    };
    let server = Server::new(
        Arc::new(bundle.runner),
        bundle.session,
        Arc::new(DefaultModeGate::new()),
    )
    .with_provider_facts(carried.clone());
    let snapshot = server.build_status_snapshot();
    assert_eq!(
        snapshot.base_url, carried.base_url,
        "the carried endpoint is what /status reports"
    );
    assert_eq!(snapshot.auth_token_source, carried.key_origin);
    remove_dir_all(&root).ok();
}

fn record(text: &str) -> PromptRecord {
    PromptRecord {
        text: text.into(),
        event_id: EventId::new(),
    }
}

fn blank_descriptor() -> SessionDescriptor {
    SessionDescriptor {
        name: None,
        name_source: NameSource::Auto,
        cwd: "/work/repo".into(),
        model: "test".into(),
        provenance: SessionProvenance::Fresh,
        version: "test".into(),
        created_at: 1000,
        child_session_ids: Vec::new(),
        first_prompt: None,
        last_prompt: None,
        prompt_coverage: PromptCoverage::Complete,
    }
}

fn seed(
    session: SessionId,
    edit: impl FnOnce(&mut SessionDescriptor),
) -> Arc<dyn SessionDescriptorStore> {
    let store: Arc<dyn SessionDescriptorStore> = Arc::new(InMemoryDescriptorStore::new());
    let mut descriptor = blank_descriptor();
    edit(&mut descriptor);
    store
        .write_descriptor(session, &descriptor)
        .expect("seed descriptor");
    store
}

fn runner_over(log: Arc<SessionStore>) -> Arc<Runner> {
    Arc::new(Runner::new(
        log,
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            ..RunnerConfig::default()
        },
    ))
}

async fn append_input(log: &SessionStore, session: SessionId, text: &str) {
    log.append(SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput { text: text.into() },
    })
    .await
    .expect("append input");
}

/// A descriptor that carries prompts titles its session from them. The log
/// here is empty, so a snapshot that still read it would report no name: the
/// prompts on the record are the only source the assertion accepts.
#[test]
fn test_snapshot_titles_from_record() {
    let session = SessionId::new();
    let store = seed(session, |descriptor| {
        descriptor.first_prompt = Some(record("research demo repo"));
        descriptor.last_prompt = Some(record("wire the cache table"));
    });
    let log = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let server = Server::new(runner_over(log), session, Arc::new(DefaultModeGate::new()))
        .with_descriptor_store(store);
    let snapshot = server.build_status_snapshot();
    let name = snapshot.descriptor.and_then(|descriptor| descriptor.name);
    assert_eq!(
        name.as_deref(),
        Some("wire the cache table"),
        "the newest prompt on the record is the name status reports"
    );
}

/// A name nobody chose says nothing about the work, so it does not outrank
/// the prompts: status titles the session by the same rule the resume listing
/// applies, and the derived name never reaches the screen.
#[test]
fn test_snapshot_ignores_auto_name() {
    let session = SessionId::new();
    let store = seed(session, |descriptor| {
        descriptor.name = Some("hello-there".into());
        descriptor.name_source = NameSource::Auto;
        descriptor.first_prompt = Some(record("research demo repo"));
        descriptor.last_prompt = Some(record("wire the cache table"));
    });
    let log = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let server = Server::new(runner_over(log), session, Arc::new(DefaultModeGate::new()))
        .with_descriptor_store(store);
    let snapshot = server.build_status_snapshot();
    assert_eq!(
        snapshot.descriptor.and_then(|descriptor| descriptor.name),
        Some("wire the cache table".to_string()),
        "a name nobody chose must not hide the prompt that names the work"
    );
}

/// A record written before prompts were recorded states no title, and its log
/// head still does: status names that session from the first input that
/// states a task, skipping the greeting that opened it.
#[tokio::test]
async fn test_snapshot_titles_from_log() {
    let session = SessionId::new();
    let store = seed(session, |_unused| {});
    // A file log is what the head read serves; the in-memory backend keeps
    // its events in a map and answers no byte range.
    let root = temp_dir().join(format!("status-log-head-{}-{}", id(), line!()));
    drop(remove_dir_all(&root));
    create_dir_all(&root).expect("mkdir root");
    let log = Arc::new(SessionStore::new(Box::new(LocalFileBackend::new(
        root.clone(),
    ))));
    append_input(&log, session, "你好").await;
    append_input(&log, session, "research demo repo").await;
    let server = Server::new(runner_over(log), session, Arc::new(DefaultModeGate::new()))
        .with_descriptor_store(store);
    let snapshot = server.build_status_snapshot();
    assert_eq!(
        snapshot.descriptor.and_then(|descriptor| descriptor.name),
        Some("research demo repo".to_string()),
        "the log head names a session whose record carries no prompt"
    );
    remove_dir_all(&root).ok();
}

/// A name the user chose outranks anything the prompts say, so renaming is
/// never undone by a later status poll.
#[test]
fn test_snapshot_keeps_user_name() {
    let session = SessionId::new();
    let store = seed(session, |descriptor| {
        descriptor.name = Some("cache work".into());
        descriptor.name_source = NameSource::User;
        descriptor.last_prompt = Some(record("wire the cache table"));
    });
    let log = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let server = Server::new(runner_over(log), session, Arc::new(DefaultModeGate::new()))
        .with_descriptor_store(store);
    let snapshot = server.build_status_snapshot();
    assert_eq!(
        snapshot.descriptor.and_then(|descriptor| descriptor.name),
        Some("cache work".to_string()),
        "the name the user chose is the name status reports"
    );
}
