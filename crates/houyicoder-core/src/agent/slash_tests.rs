use super::*;
use crate::agent::ConditionalSkillActivator;

#[test]
fn test_parse_plain_skill() {
    let (name, args) = parse_skill_slash("@skill:commit").unwrap();
    assert_eq!(name, "commit");
    assert!(args.is_none());
}

#[test]
fn test_parse_skill_with_args() {
    let (name, args) = parse_skill_slash("@skill:commit fix typo").unwrap();
    assert_eq!(name, "commit");
    assert_eq!(args, Some("fix typo"));
}

#[test]
fn test_parse_path_rejected() {
    // The "/" in "home/you" fails the skill-name charset.
    assert!(parse_skill_slash("@skill:home/you").is_none());
}

#[test]
fn test_parse_rejects_wrong_prefix() {
    assert!(parse_skill_slash("/commit").is_none());
    assert!(parse_skill_slash("@file:commit").is_none());
}

#[test]
fn test_parse_rejects_uppercase() {
    assert!(parse_skill_slash("@skill:Commit").is_none());
}

#[test]
fn test_parse_rejects_no_prefix() {
    assert!(parse_skill_slash("commit fix typo").is_none());
    assert!(parse_skill_slash(" plain text").is_none());
}

#[test]
fn test_parse_rejects_empty_name() {
    assert!(parse_skill_slash("@skill:").is_none());
    assert!(parse_skill_slash("@skill: args").is_none());
}

#[test]
fn test_parse_dash_digit() {
    let (name, _) = parse_skill_slash("@skill:review-pr-2").unwrap();
    assert_eq!(name, "review-pr-2");
}

// ---- resolve_skill_slash method ----

use houyicoder_api::skill::{SkillDescriptor, SkillHookSpec, SkillRegistry};
use houyicoder_api::trust::TrustState;
use houyicoder_memory::InMemoryBackend;
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;
use std::sync::Arc;
use std::sync::RwLock;

use crate::agent::SkillHookRegistrar;
use crate::agent::{HookEvent, HookPayload, HookRegistry, ToolResult};

/// A stub registry: "commit" is user-invocable + echoes args, "secret"
/// is not user-invocable, anything else NotFound.
struct SlashStubRegistry;
impl SkillRegistry for SlashStubRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        Vec::new()
    }
    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        let user_invocable = match name {
            "commit" => true,
            "secret" => false,
            _ => return None,
        };
        Some(SkillDescriptor {
            name: name.to_string(),
            description: format!("desc for {name}"),
            when_to_use: None,
            argument_hint: None,
            disable_model_invocation: false,
            user_invocable,
            body_token_estimate: 0,
            allowed_tools: Vec::new(),
            allowed_mach_services: Vec::new(),
        })
    }
    fn prepare_body(
        &self,
        name: &str,
        args: Option<&str>,
        _sid: Option<&str>,
    ) -> Result<String, SkillError> {
        // Ungated: resolve_skill_slash gates via find; this returns the
        // body for any known skill.
        match name {
            "commit" => Ok(format!("commit body: {}", args.unwrap_or(""))),
            "secret" => Ok("secret body".into()),
            _ => Err(SkillError::NotFound(name.into())),
        }
    }
}

fn runner_with_slash() -> Runner {
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Runner::with_shared_store(
        store,
        Arc::new(crate::provider::test_support::FakeProvider::text("done")),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry::default(),
        },
    )
    .with_skill_registry(Arc::new(SlashStubRegistry))
}

#[tokio::test]
async fn test_resolve_known_skill() {
    let runner = runner_with_slash();
    let outcome = runner
        .resolve_skill_slash(SessionId::new(), "@skill:commit fix typo")
        .await;
    match outcome {
        SkillSlashOutcome::Prepared {
            name,
            body,
            untrusted,
        } => {
            assert_eq!(name, "commit", "name carried: {name}");
            assert!(body.contains("commit body: fix typo"), "{body}");
            // The stub leaves list_with_origin at the default (empty),
            // so the origin scan finds nothing and defaults to
            // untrusted=true (fail-closed for an unknown source).
            assert!(untrusted, "unknown origin defaults to untrusted");
        }
        other => panic!("expected Prepared, got {other:?}"),
    }
}

#[tokio::test]
async fn test_resolve_unknown() {
    let runner = runner_with_slash();
    let outcome = runner
        .resolve_skill_slash(SessionId::new(), "@skill:nope")
        .await;
    assert!(
        matches!(outcome, SkillSlashOutcome::NotASkill),
        "an unknown token is not a skill (falls through)"
    );
}

#[tokio::test]
async fn test_resolve_refused_returns_notice() {
    let runner = runner_with_slash();
    let outcome = runner
        .resolve_skill_slash(SessionId::new(), "@skill:secret")
        .await;
    match outcome {
        SkillSlashOutcome::Refused(notice) => assert!(notice.contains("not"), "{notice}"),
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[tokio::test]
async fn test_resolve_plain_text() {
    let runner = runner_with_slash();
    let outcome = runner
        .resolve_skill_slash(SessionId::new(), "just a message, no slash")
        .await;
    assert!(
        matches!(outcome, SkillSlashOutcome::NotASkill),
        "plain text is not a skill slash"
    );
}

#[tokio::test]
async fn test_resolve_noop_without_registry() {
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::with_shared_store(
        store,
        Arc::new(crate::provider::test_support::FakeProvider::text("done")),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry::default(),
        },
    );
    let outcome = runner
        .resolve_skill_slash(SessionId::new(), "@skill:commit")
        .await;
    assert!(
        matches!(outcome, SkillSlashOutcome::NotASkill),
        "no registry wired -> not a skill"
    );
}

/// A full run() with @skill:name lands the raw text as UserInput AND
/// the prepared body as a MetaUser — the transcript shows what the
/// user typed, the model reads the body as a directive. Covers the
/// run() entry integration (resolve + append SkillBody).
#[tokio::test]
async fn test_run_lands_skill_body() {
    use houyicoder_context::TurnEventKind;
    let runner = runner_with_slash();
    let session = SessionId::new();
    runner
        .run(session, "@skill:commit fix typo".into())
        .await
        .expect("run completes");
    let view = runner.store().current_view(session).await.unwrap();
    // The raw @skill: text is the UserInput (what the user typed).
    let user_text = view.events.iter().find_map(|e| match &e.kind {
        TurnEventKind::UserInput { text } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(
        user_text.as_deref(),
        Some("@skill:commit fix typo"),
        "raw @skill: text kept"
    );
    // The prepared body lands as a durable SkillBody (not a MetaUser, so
    // it survives a compaction boundary).
    let body = view.events.iter().find_map(|e| match &e.kind {
        TurnEventKind::SkillBody {
            skill_name,
            content,
            ..
        } => Some((skill_name.clone(), content.clone())),
        _ => None,
    });
    let (name, content) = body.expect("a SkillBody with the prepared body was appended");
    assert_eq!(name, "commit", "skill_name carried: {name}");
    assert!(
        content.contains("commit body: fix typo"),
        "body in SkillBody: {content}"
    );
}

/// A refused skill (@skill:secret, user-invocable=false) ends the turn
/// without a model call: no MetaUser body appended, no assistant
/// message, turns=0. The refusal surfaces as a system line (no-op in
/// tests with no live sink).
#[tokio::test]
async fn test_run_refused_skips_model() {
    use houyicoder_context::TurnEventKind;
    let runner = runner_with_slash();
    let session = SessionId::new();
    let result = runner
        .run(session, "@skill:secret".into())
        .await
        .expect("run completes");
    assert_eq!(result.turns, 0, "no model turns for a refused skill");
    assert!(
        matches!(result.outcome, crate::agent::RunOutcome::FinalOutput(_)),
        "turn ended without a model call"
    );
    let view = runner.store().current_view(session).await.unwrap();
    // The raw @skill: text is kept (the user sees what they typed).
    assert!(view.events.iter().any(|e| matches!(
        e.kind,
        TurnEventKind::UserInput { ref text } if text == "@skill:secret"
    )));
    // No SkillBody + no assistant message: the model never ran.
    assert!(
        !view.events.iter().any(|e| matches!(
            e.kind,
            TurnEventKind::SkillBody { .. } | TurnEventKind::AssistantMessage { .. }
        )),
        "refusal skips the model (no SkillBody, no assistant message)"
    );
}

/// A stub registry whose hooks_for returns a single Managed spec, so a
/// @skill: dispatch registers a hook. prepare_body returns a body for
/// "commit".
struct HookStubRegistry;
impl SkillRegistry for HookStubRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        Vec::new()
    }
    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        if name == "commit" {
            Some(SkillDescriptor {
                name: name.into(),
                description: "d".into(),
                when_to_use: None,
                argument_hint: None,
                disable_model_invocation: false,
                user_invocable: true,
                body_token_estimate: 0,
                allowed_tools: Vec::new(),
                allowed_mach_services: Vec::new(),
            })
        } else {
            None
        }
    }
    fn prepare_body(
        &self,
        name: &str,
        _args: Option<&str>,
        _sid: Option<&str>,
    ) -> Result<String, SkillError> {
        match name {
            "commit" => Ok("body".into()),
            _ => Err(SkillError::NotFound(name.into())),
        }
    }
    fn hooks_for(&self, _name: &str) -> Vec<SkillHookSpec> {
        vec![SkillHookSpec {
            event: "PostToolUse".into(),
            // Matcher targets a different tool than the dispatch ctx
            // (bash), so the hook returns Allow without spawning — the
            // timing test asserts registration, not the spawn path.
            matcher: Some("Write".into()),
            command: "echo".into(),
            args: vec![],
            once: false,
            if_rule: None,
            source: houyicoder_api::skill::HookSourceKind::Managed,
        }]
    }
}

/// Registration is invoke-time, not discovery-time: a skill whose
/// hooks_for returns a spec does not register until the @skill: dispatch
/// resolves it. Before invoke the hook registry is empty; after, dispatch
/// fires the registered hook.
#[tokio::test]
async fn test_invoke_registers_hook_timing() {
    let hook_reg = Arc::new(HookRegistry::new());
    let trust = Arc::new(RwLock::new(TrustState::Trusted));
    let launcher: Arc<dyn houyicoder_api::launcher::ProcessLauncher> =
        Arc::new(houyicoder_api::launcher::StdProcessLauncher::new());
    let registrar = Arc::new(SkillHookRegistrar::new(hook_reg.clone(), trust, launcher));
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::with_shared_store(
        store,
        Arc::new(crate::provider::test_support::FakeProvider::text("done")),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry::default(),
        },
    )
    .with_skill_registry(Arc::new(HookStubRegistry))
    .with_hooks(hook_reg.clone())
    .with_registrar(registrar);
    // Before invoke: discovery does not register, dispatch fires nothing.
    assert!(hook_reg.is_empty(), "discovery does not register hooks");
    let outcome = runner
        .resolve_skill_slash(SessionId::new(), "@skill:commit")
        .await;
    assert!(matches!(outcome, SkillSlashOutcome::Prepared { .. }));
    // After invoke: the spec registered, dispatch fires it.
    let ctx = crate::agent::HookContext {
        event: HookEvent::PostToolUse,
        payload: HookPayload::PostToolUse {
            tool_name: "bash".into(),
            input: serde_json::json!({}),
            result: ToolResult {
                output: "{}".into(),
            },
        },
        session: SessionId::new(),
    };
    assert_eq!(
        hook_reg.dispatch(&ctx).len(),
        1,
        "invoke registered the hook"
    );
}

/// A registry with one paths-gated, user-invocable skill.
struct PathsSlashRegistry;
impl SkillRegistry for PathsSlashRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        vec![paths_descriptor("gated")]
    }
    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        (name == "gated").then(|| paths_descriptor("gated"))
    }
    fn prepare_body(
        &self,
        _name: &str,
        _args: Option<&str>,
        _sid: Option<&str>,
    ) -> Result<String, SkillError> {
        Ok("gated body".into())
    }
    fn paths_for(&self, name: &str) -> Vec<String> {
        if name == "gated" {
            vec!["src".to_string()]
        } else {
            Vec::new()
        }
    }
}

fn paths_descriptor(name: &str) -> SkillDescriptor {
    SkillDescriptor {
        name: name.to_string(),
        description: "d".into(),
        when_to_use: None,
        argument_hint: None,
        disable_model_invocation: false,
        user_invocable: true,
        body_token_estimate: 0,
        allowed_tools: Vec::new(),
        allowed_mach_services: Vec::new(),
    }
}

fn runner_with_paths(activator: Arc<dyn crate::agent::ConditionalSkillActivator>) -> Runner {
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Runner::with_shared_store(
        store,
        Arc::new(crate::provider::test_support::FakeProvider::text("done")),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry::default(),
        },
    )
    .with_skill_registry(Arc::new(PathsSlashRegistry))
    .with_conditional(activator)
}

/// A conditional skill refuses via slash until activated; the message
/// names the paths.
#[tokio::test]
async fn test_slash_conditional_refuses() {
    let reg: Arc<dyn houyicoder_api::skill::SkillRegistry> = Arc::new(PathsSlashRegistry);
    let cwd = std::env::temp_dir().join("houyi-slash-refuse");
    let activator = Arc::new(crate::agent::ConditionalActivation::new(reg, cwd));
    let runner = runner_with_paths(activator);
    let outcome = runner
        .resolve_skill_slash(SessionId::new(), "@skill:gated")
        .await;
    match outcome {
        SkillSlashOutcome::Refused(m) => {
            assert!(m.contains("conditional"), "{m}");
            assert!(m.contains("src"), "message names paths: {m}");
        }
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[tokio::test]
async fn test_slash_conditional_passes() {
    let reg: Arc<dyn houyicoder_api::skill::SkillRegistry> = Arc::new(PathsSlashRegistry);
    let cwd = std::env::temp_dir().join("houyi-slash-pass");
    let activator = Arc::new(crate::agent::ConditionalActivation::new(reg, cwd));
    // Activate via a matching file, then slash reaches the body.
    activator.activate_for_paths(&["src/foo.rs".to_string()]);
    let runner = runner_with_paths(activator);
    let outcome = runner
        .resolve_skill_slash(SessionId::new(), "@skill:gated")
        .await;
    match outcome {
        SkillSlashOutcome::Prepared { name, body, .. } => {
            assert_eq!(name, "gated");
            assert!(body.contains("gated body"), "{body}");
        }
        other => panic!("expected Prepared, got {other:?}"),
    }
}

/// A prepare_body error (BodyLoad) records a refusal via
/// record_invocation. The stub tracks the call so the test proves
/// the error path is wired, not silently skipped.
#[tokio::test]
async fn test_slash_load_error_refusal() {
    use std::sync::atomic::{AtomicU64, Ordering};

    struct LoadFailRegistry {
        refused: AtomicU64,
    }
    impl SkillRegistry for LoadFailRegistry {
        fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
            Vec::new()
        }
        fn find(&self, name: &str) -> Option<SkillDescriptor> {
            (name == "broken").then(|| SkillDescriptor {
                name: "broken".into(),
                description: "d".into(),
                when_to_use: None,
                argument_hint: None,
                disable_model_invocation: false,
                user_invocable: true,
                body_token_estimate: 0,
                allowed_tools: Vec::new(),
                allowed_mach_services: Vec::new(),
            })
        }
        fn prepare_body(
            &self,
            _name: &str,
            _args: Option<&str>,
            _sid: Option<&str>,
        ) -> Result<String, SkillError> {
            Err(SkillError::BodyLoad("disk gone".into()))
        }
        fn record_invocation(&self, _name: &str, refused: bool) {
            if refused {
                self.refused.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let reg = Arc::new(LoadFailRegistry {
        refused: AtomicU64::new(0),
    });
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::with_shared_store(
        store,
        Arc::new(crate::provider::test_support::FakeProvider::text("done")),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry::default(),
        },
    )
    .with_skill_registry(reg.clone());
    let outcome = runner
        .resolve_skill_slash(SessionId::new(), "@skill:broken")
        .await;
    assert!(matches!(outcome, SkillSlashOutcome::Refused(_)));
    assert_eq!(
        reg.refused.load(Ordering::Relaxed),
        1,
        "load error records a refusal"
    );
}

/// Session context is injected into a skill body containing the
/// {{userMessages}} and {{sessionMemory}} placeholders. The user
/// messages from the session view replace {{userMessages}}; the session
/// memory placeholder gets a default string (integration is follow-up).
#[tokio::test]
async fn test_session_context_injection() {
    struct TemplateRegistry;
    impl houyicoder_api::skill::SkillRegistry for TemplateRegistry {
        fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
            Vec::new()
        }
        fn find(&self, name: &str) -> Option<SkillDescriptor> {
            (name == "template").then(|| SkillDescriptor {
                name: "template".into(),
                description: "d".into(),
                when_to_use: None,
                argument_hint: None,
                disable_model_invocation: false,
                user_invocable: true,
                body_token_estimate: 0,
                allowed_tools: Vec::new(),
                allowed_mach_services: Vec::new(),
            })
        }
        fn prepare_body(
            &self,
            _name: &str,
            _args: Option<&str>,
            _sid: Option<&str>,
        ) -> Result<String, SkillError> {
            Ok("Description: {{userDescription}}\nUser messages:\n{{userMessages}}\nSession memory:\n{{sessionMemory}}".into())
        }
        fn paths_for(&self, _name: &str) -> Vec<String> {
            Vec::new()
        }
    }

    let reg: Arc<dyn houyicoder_api::skill::SkillRegistry> = Arc::new(TemplateRegistry);
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::with_shared_store(
        store,
        Arc::new(crate::provider::test_support::FakeProvider::text("done")),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry::default(),
        },
    )
    .with_skill_registry(reg);
    let session = SessionId::new();
    runner
        .append_user_input(session, "hello world".to_string())
        .await
        .unwrap();
    let outcome = runner
        .resolve_skill_slash(session, "@skill:template capture workflow")
        .await;
    match outcome {
        SkillSlashOutcome::Prepared { body, .. } => {
            assert!(
                body.contains("hello world"),
                "user messages injected: {body}"
            );
            assert!(
                !body.contains("{{userMessages}}"),
                "placeholder replaced: {body}"
            );
            assert!(
                body.contains("No session memory available"),
                "session memory placeholder: {body}"
            );
            assert!(
                body.contains("capture workflow"),
                "user description injected: {body}"
            );
        }
        other => panic!("expected Prepared, got {other:?}"),
    }
}
