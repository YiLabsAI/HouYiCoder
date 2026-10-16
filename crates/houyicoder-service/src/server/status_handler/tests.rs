//! Tests for session title derivation from the log head.

use super::*;
use crate::composition::BuildRunnerOptions;
use crate::composition::build_runner;
use houyicoder_api::session::SessionLog;
use houyicoder_context::SessionId;
use houyicoder_context::{EventId, SessionEvent, SessionLogEntry};
use houyicoder_memory::LocalFileBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_provider::ProviderFacts;
use houyicoder_provider::ProviderResolution;
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::process::id;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Compact title formatting turns a prompt into a clean session name.
#[test]
fn test_compact_session_title() {
    assert_eq!(compact_session_title("Fix login bug"), "fix-login-bug");
    assert_eq!(
        compact_session_title("  Refactor  the  spec  strip  "),
        "refactor-the-spec-strip"
    );
    let long = compact_session_title("a".repeat(80).as_str());
    assert_eq!(long.chars().count(), 40);
    assert!(
        long.ends_with('\u{2026}'),
        "truncated title ends with ellipsis: {long}"
    );
}

#[tokio::test]
async fn test_first_prompt_title_log() {
    let root = temp_dir().join(format!(
        "title-test-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    store
        .append(SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "research demo repo".into(),
            },
        })
        .await
        .unwrap();
    let log: &dyn SessionLog = &store as &dyn SessionLog;
    let title = first_prompt_title(log, sid);
    assert_eq!(title.as_deref(), Some("research-demo-repo"));
    remove_dir_all(&root).ok();
}

#[tokio::test]
async fn test_first_prompt_title_absent() {
    let root = temp_dir().join(format!(
        "title-empty-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    let log: &dyn SessionLog = &store as &dyn SessionLog;
    assert!(first_prompt_title(log, sid).is_none());
    remove_dir_all(&root).ok();
}

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
