//! Tests for this module.

use super::*;
use crate::agent::conditional_activation::ConditionalSkillActivator;

fn descriptor(name: &str, desc: &str, when: Option<&str>) -> SkillDescriptor {
    SkillDescriptor {
        name: name.to_string(),
        description: desc.to_string(),
        when_to_use: when.map(String::from),
        argument_hint: None,
        disable_model_invocation: false,
        user_invocable: true,
        body_token_estimate: 0,
        allowed_tools: Vec::new(),
        allowed_mach_services: Vec::new(),
        allow_app_launch: false,
    }
}

#[test]
fn test_empty_returns_empty() {
    assert_eq!(format_skill_listing(&[], 200_000), "");
}

#[test]
fn test_full_listing_within_budget() {
    let descs = vec![
        descriptor("commit", "commit changes", Some("after edits")),
        descriptor("review", "review a PR", None),
    ];
    let out = format_skill_listing(&descs, 200_000);
    assert!(
        out.contains("- commit: commit changes - after edits"),
        "{out}"
    );
    assert!(out.contains("- review: review a PR"), "{out}");
    assert_eq!(out.matches('\n').count(), 1, "two entries, one newline");
}

/// The listing shows each skill body token estimate so the model sees
/// the invocation cost before committing. A skill with zero estimate
/// (unknown body) shows no cost suffix.
#[test]
fn test_listing_shows_token_cost() {
    let d = SkillDescriptor {
        name: "big".to_string(),
        description: "a big skill".to_string(),
        when_to_use: None,
        argument_hint: None,
        disable_model_invocation: false,
        user_invocable: true,
        body_token_estimate: 4200,
        allowed_tools: Vec::new(),
        allowed_mach_services: Vec::new(),
        allow_app_launch: false,
    };
    let out = format_skill_listing(&[d], 200_000);
    assert!(
        out.contains("(~4200 tok)"),
        "body token estimate in the listing: {out}"
    );
}

#[test]
fn test_when_to_use_appended() {
    let out = format_skill_listing(
        &[descriptor("commit", "commit changes", Some("after edits"))],
        200_000,
    );
    assert!(
        out.contains("commit changes - after edits"),
        "when_to_use appended with dash: {out}"
    );
}

#[test]
fn test_entry_cap_truncates() {
    let long = "x".repeat(300);
    let out = format_skill_listing(&[descriptor("big", &long, None)], 200_000);
    let entry = out.strip_prefix("- big: ").unwrap_or(&out);
    assert!(
        entry.chars().count() <= MAX_LISTING_DESC_CHARS,
        "entry within cap: {} > {MAX_LISTING_DESC_CHARS}",
        entry.chars().count()
    );
    assert!(entry.ends_with("..."), "truncated with ellipsis: {entry}");
}

#[test]
fn test_truncates_under_tight_budget() {
    // window 1000 -> budget 40; names+overhead exceed it, so
    // descriptions drop to names-only and names survive.
    let long = "x".repeat(500);
    let descs = vec![
        descriptor("alpha", &long, None),
        descriptor("beta", &long, None),
    ];
    let out = format_skill_listing(&descs, 1_000);
    assert!(out.contains("- alpha"), "name survives: {out}");
    assert!(out.contains("- beta"), "name survives: {out}");
    assert!(
        !out.contains(": x"),
        "descriptions dropped under extreme budget: {out}"
    );
}

#[test]
fn test_names_only_extreme_budget() {
    let descs = vec![
        descriptor("alpha", "do alpha thing", None),
        descriptor("beta", "do beta thing", None),
    ];
    // window 100 -> budget 4 -> names-only.
    let out = format_skill_listing(&descs, 100);
    assert!(out.contains("- alpha"), "{out}");
    assert!(out.contains("- beta"), "{out}");
    assert!(!out.contains(": do"), "descriptions dropped: {out}");
}

#[test]
fn test_zero_window_default_budget() {
    // context_window 0 -> default 8000 char budget; a small set fits.
    let descs = vec![descriptor("commit", "commit changes", None)];
    let out = format_skill_listing(&descs, 0);
    assert!(out.contains("- commit: commit changes"), "{out}");
}

// ---- inject_skill_listing method tests ----
//
// These need a Runner, so they build one with a stub provider + a stub
// registry, mirroring the memory_gates pattern. The formatter is covered
// above; these cover the scan-skip + append-when-absent paths.

use houyicoder_api::provider::stream_from_response;
use houyicoder_api::skill::SkillError;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{
    CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;
use std::sync::Arc;

/// A stub provider: only capabilities() is read at construction.
struct StubProvider;
impl houyicoder_api::provider::ModelProvider for StubProvider {
    fn complete(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async {
            Ok(CompletionResponse {
                output: vec![OutputItem::Text {
                    text: "done".into(),
                }],
                usage: houyicoder_protocol::llm::Usage::default(),
                model: "test".into(),
            })
        })
    }
    fn stream(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
        stream_from_response(CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: houyicoder_protocol::llm::Usage::default(),
            model: "test".into(),
        })
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A stub registry that returns one model-invocable skill.
struct OneSkillRegistry;
impl houyicoder_api::skill::SkillRegistry for OneSkillRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        vec![descriptor("commit", "commit changes", None)]
    }
    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        if name == "commit" {
            Some(descriptor("commit", "commit changes", None))
        } else {
            None
        }
    }
    fn prepare_body(
        &self,
        _name: &str,
        _args: Option<&str>,
        _session_id: Option<&str>,
    ) -> Result<String, SkillError> {
        Ok("body".into())
    }
}

/// A registry whose descriptor changes after the first read, to simulate a
/// skill-set refresh (hot reload). The first call returns one description;
/// every later call returns a different one, so the content hash flips.
struct ChangingSkillRegistry {
    calls: std::sync::atomic::AtomicU32,
}
impl houyicoder_api::skill::SkillRegistry for ChangingSkillRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let desc = if n == 0 {
            "commit changes"
        } else {
            "commit changes v2"
        };
        vec![descriptor("commit", desc, None)]
    }
    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        if name == "commit" {
            Some(descriptor("commit", "commit changes", None))
        } else {
            None
        }
    }
    fn prepare_body(
        &self,
        _name: &str,
        _args: Option<&str>,
        _session_id: Option<&str>,
    ) -> Result<String, SkillError> {
        Ok("body".into())
    }
}

fn runner_with_changing_skills() -> (Runner, SessionId) {
    use houyicoder_context::SessionId;
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::new(
        store,
        Arc::new(StubProvider),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry::default(),
        },
    )
    .with_skill_registry(Arc::new(ChangingSkillRegistry {
        calls: std::sync::atomic::AtomicU32::new(0),
    }));
    let session = SessionId::new();
    (runner, session)
}

fn runner_with_skills() -> (Runner, SessionId) {
    use houyicoder_context::SessionId;
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::new(
        store,
        Arc::new(StubProvider),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry::default(),
        },
    )
    .with_skill_registry(Arc::new(OneSkillRegistry));
    let session = SessionId::new();
    (runner, session)
}

/// When no listing survives in the view, inject appends a SkillListing
/// event the assembly merges into the user message.
#[tokio::test]
async fn test_inject_appends_when_absent() {
    let (runner, session) = runner_with_skills();
    runner.inject_skill_listing(session).await.unwrap();
    let view = runner.store().current_view(session).await.unwrap();
    let listing = view.events.iter().find_map(|e| match &e.event {
        SessionEvent::SkillListing { text, .. } => Some(text.clone()),
        _ => None,
    });
    let text = listing.expect("a SkillListing event was appended");
    assert!(text.contains("- commit: commit changes"), "{text}");
}

/// When a listing already survives in the view, inject is a no-op: the
/// event count does not grow. This is the dedup that makes repeat calls
/// at compact re-drive sites safe.
#[tokio::test]
async fn test_inject_skips_when_present() {
    let (runner, session) = runner_with_skills();
    runner.inject_skill_listing(session).await.unwrap();
    let after_first = runner
        .store()
        .current_view(session)
        .await
        .unwrap()
        .events
        .len();
    runner.inject_skill_listing(session).await.unwrap();
    let after_second = runner
        .store()
        .current_view(session)
        .await
        .unwrap()
        .events
        .len();
    assert_eq!(
        after_first, after_second,
        "second inject is a no-op when a listing survives"
    );
}

/// A content-hash mismatch — the descriptor set changed since the
/// surviving listing was announced (here a simulated registry refresh;
/// in production a hot-reload invalidation) — re-announces instead of
/// skipping. A body-only edit that leaves the listing text unchanged
/// does not flip the hash, so it does not re-announce; a description
/// change does.
#[tokio::test]
async fn test_re_announces_on_change() {
    let (runner, session) = runner_with_changing_skills();
    runner.inject_skill_listing(session).await.unwrap();
    let after_first: Vec<_> = runner
        .store()
        .current_view(session)
        .await
        .unwrap()
        .events
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::SkillListing {
                text, content_hash, ..
            } => Some((text.clone(), *content_hash)),
            _ => None,
        })
        .collect();
    assert_eq!(after_first.len(), 1, "first inject appends one listing");
    assert!(
        after_first[0].0.contains("commit changes"),
        "{:?}",
        after_first
    );
    let first_hash = after_first[0].1;

    // Second inject: the registry now returns a different description, so
    // the content hash flips and the listing re-announces.
    runner.inject_skill_listing(session).await.unwrap();
    let after_second: Vec<_> = runner
        .store()
        .current_view(session)
        .await
        .unwrap()
        .events
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::SkillListing {
                text, content_hash, ..
            } => Some((text.clone(), *content_hash)),
            _ => None,
        })
        .collect();
    assert_eq!(
        after_second.len(),
        2,
        "changed set re-announces: {:?}",
        after_second
    );
    assert!(
        after_second[1].0.contains("commit changes v2"),
        "re-announce carries the new description: {:?}",
        after_second
    );
    assert_ne!(
        after_second[1].1, first_hash,
        "re-announced listing has a new content hash"
    );
}

/// When no registry is wired, inject is a no-op (no event appended).
#[tokio::test]
async fn test_inject_noop_without_registry() {
    use houyicoder_context::SessionId;
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    // No with_skill_registry: skill_registry is None.
    let runner = Runner::new(
        store.clone(),
        Arc::new(StubProvider),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry::default(),
        },
    );
    let session = SessionId::new();
    runner.inject_skill_listing(session).await.unwrap();
    let view = store.current_view(session).await.unwrap();
    assert!(
        view.events
            .iter()
            .all(|e| !matches!(e.event, SessionEvent::SkillListing { .. })),
        "no listing appended without a registry"
    );
}

/// A registry carrying one unconditional + one paths-gated skill.
struct ConditionalRegistry {
    skills: Vec<(String, Vec<String>)>,
}
impl ConditionalRegistry {
    fn new(skills: &[(&str, &[&str])]) -> Self {
        Self {
            skills: skills
                .iter()
                .map(|(n, p)| (n.to_string(), p.iter().map(|s| s.to_string()).collect()))
                .collect(),
        }
    }
}
impl houyicoder_api::skill::SkillRegistry for ConditionalRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        self.skills
            .iter()
            .map(|(n, _)| descriptor(n, "d", None))
            .collect()
    }
    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        self.skills
            .iter()
            .find(|(n, _)| n == name)
            .map(|(n, _)| descriptor(n, "d", None))
    }
    fn prepare_body(
        &self,
        _name: &str,
        _args: Option<&str>,
        _session_id: Option<&str>,
    ) -> Result<String, SkillError> {
        Ok("body".into())
    }
    fn paths_for(&self, name: &str) -> Vec<String> {
        self.skills
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, p)| p.clone())
            .unwrap_or_default()
    }
}

#[tokio::test]
async fn test_conditional_hidden_until_active() {
    let registry: Arc<dyn houyicoder_api::skill::SkillRegistry> =
        Arc::new(ConditionalRegistry::new(&[
            ("always", &[]),
            ("gated", &["src"]),
        ]));
    let cwd = std::env::temp_dir().join("houyi-listing-test");
    let activator = Arc::new(
        crate::agent::conditional_activation::ConditionalActivation::new(
            std::sync::Arc::clone(&registry),
            cwd,
        ),
    );
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::new(
        store.clone(),
        Arc::new(StubProvider),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry::default(),
        },
    )
    .with_skill_registry(registry)
    .with_conditional(std::sync::Arc::clone(&activator)
        as Arc<dyn crate::agent::conditional_activation::ConditionalSkillActivator>);
    let session = SessionId::new();

    // Before activation: only the unconditional skill is listed.
    runner.inject_skill_listing(session).await.unwrap();
    let text = store
        .current_view(session)
        .await
        .unwrap()
        .events
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::SkillListing { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("listing appended");
    assert!(text.contains("- always"), "unconditional shown: {text}");
    assert!(
        !text.contains("gated"),
        "gated hidden before activation: {text}"
    );

    // After a matching file-touch the gated skill activates + the
    // content-hash change re-announces with it visible.
    activator.activate_for_paths(&["src/foo.rs".to_string()]);
    runner.inject_skill_listing(session).await.unwrap();
    let last = store
        .current_view(session)
        .await
        .unwrap()
        .events
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEvent::SkillListing { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("re-announced listing");
    assert!(
        last.contains("gated"),
        "gated shown after activation: {last}"
    );
    assert!(last.contains("always"), "unconditional still shown: {last}");
}
