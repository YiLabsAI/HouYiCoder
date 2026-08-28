//! agent::skill_slash — the slash-command skill dispatch path.
//!
//! When the user types /skill-name args, the raw text reaches the service
//! as a UserInput (the TUI is a pure protocol client and sends /-prefixed
//! text through unchanged when no builtin matches). This module resolves
//! the slash prefix against the skill registry, prepares the body (the
//! same body-prep function the Skill tool uses — the two paths converge
//! there), and returns a MetaUser text to append alongside the raw
//! UserInput so the transcript shows what the user typed while the model
//! reads the prepared body as a directive.
//!
//! The raw UserInput is kept (the transcript shows "/skill-name args"); the
//! prepared body lands as a MetaUser the projection merges into the turn's
//! user message (transcript-skipped in brief mode, like the memory-recall
//! attachment). disable-model-invocation skills hidden from the model are
//! reachable here when user-invocable is true; the gate is user-invocable,
//! not disable-model-invocation.

use houyicoder_api::skill::SkillError;
use houyicoder_context::SessionId;

use super::Runner;

/// Parse a /-prefixed input into (skill name, optional args). Returns
/// None when the input is not a /-prefix or the first token is not a
/// valid skill name (^[a-z0-9-]+$), so paths like /home/you and unknown
/// /-tokens fall through to normal UserInput handling.
///
/// The name is validated against the skill-name charset so a path
/// (/etc/passwd) or a mixed-case token (/Nope) never resolves as a skill
/// name and reaches the registry only when it could be one. The registry
/// has the final say (a valid-shape name with no matching skill returns
/// NotASkill).
fn parse_skill_slash(text: &str) -> Option<(String, Option<&str>)> {
    let text = text.trim();
    let rest = text.strip_prefix('/')?;
    if rest.starts_with('/') {
        return None;
    }
    let mut split = rest.splitn(2, char::is_whitespace);
    let name = split.next()?;
    if name.is_empty() {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    let args = split.next().map(str::trim).filter(|s| !s.is_empty());
    Some((name.to_string(), args))
}

/// The outcome of resolving a user input as a skill slash.
#[derive(Debug)]
pub(crate) enum SkillSlashOutcome {
    /// Not a /-skill (a path, an unknown /-token, or no registry wired).
    /// The run proceeds with the raw UserInput only.
    NotASkill,
    /// A user-invocable skill: append the prepared body as a durable
    /// SkillBody the model reads as a directive (and that survives a
    /// compaction boundary, unlike a MetaUser which compaction folds).
    Prepared { name: String, body: String },
    /// A skill that is not user-invocable. Surface the notice to the user
    /// as a system line and end the turn without a model call — the model
    /// has nothing to do for a refused skill.
    Refused(String),
}

impl Runner {
    /// Resolve a /-prefixed user input as a skill slash. The caller
    /// (run() entry) appends the raw /-text as UserInput first, then
    /// handles the outcome: Prepared appends a MetaUser body; Refused
    /// surfaces a system line + skips the model call; NotASkill falls
    /// through to the normal run.
    ///
    /// Gated on user-invocable, not disable-model-invocation: a skill
    /// hidden from the model is reachable via slash when user-invocable is
    /// true. The body preparation shares the same pure function the Skill
    /// tool uses (the two invocation paths converge there).
    pub(crate) async fn resolve_skill_slash(
        &self,
        session: SessionId,
        text: &str,
    ) -> SkillSlashOutcome {
        let Some(registry) = self.skill_registry.as_ref() else {
            return SkillSlashOutcome::NotASkill;
        };
        let Some((name, args)) = parse_skill_slash(text) else {
            return SkillSlashOutcome::NotASkill;
        };
        let sid = session.to_string();
        // Gate on user-invocable (the slash path): find returns the
        // descriptor + its flag; the shared prepare_body is ungated so a
        // model-disabled but user-invocable skill is reachable here.
        let desc = match registry.find(&name) {
            None => return SkillSlashOutcome::NotASkill,
            Some(d) => d,
        };
        if !desc.user_invocable {
            return SkillSlashOutcome::Refused(format!(
                "The skill \"{name}\" cannot be invoked directly by the user. \
                 Ask the assistant to use the {name} skill for you."
            ));
        }
        match registry.prepare_body(&name, args, Some(&sid)) {
            Ok(body) => SkillSlashOutcome::Prepared { name, body },
            Err(SkillError::NotFound(_)) => SkillSlashOutcome::NotASkill,
            Err(other) => SkillSlashOutcome::Refused(format!("skill invocation failed: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_plain_skill() {
        let (name, args) = parse_skill_slash("/commit").unwrap();
        assert_eq!(name, "commit");
        assert!(args.is_none());
    }

    #[test]
    fn test_parse_skill_with_args() {
        let (name, args) = parse_skill_slash("/commit fix typo").unwrap();
        assert_eq!(name, "commit");
        assert_eq!(args, Some("fix typo"));
    }

    #[test]
    fn test_parse_path_rejected() {
        // /home/you has no whitespace, so the whole "home/you" is one
        // token; the '/' fails the skill-name charset and the parser
        // rejects it — paths never reach the registry.
        assert!(parse_skill_slash("/home/you").is_none());
    }

    #[test]
    fn test_parse_rejects_double_slash() {
        assert!(parse_skill_slash("//notaskill").is_none());
    }

    #[test]
    fn test_parse_rejects_uppercase() {
        // Skill names are ^[a-z0-9-]+$; mixed case is not a skill name.
        assert!(parse_skill_slash("/Commit").is_none());
    }

    #[test]
    fn test_parse_rejects_no_prefix() {
        assert!(parse_skill_slash("commit fix typo").is_none());
        assert!(parse_skill_slash(" plain text").is_none());
    }

    #[test]
    fn test_parse_rejects_empty_name() {
        assert!(parse_skill_slash("/").is_none());
        assert!(parse_skill_slash("/ args").is_none());
    }

    #[test]
    fn test_parse_dash_digit() {
        let (name, _) = parse_skill_slash("/review-pr-2").unwrap();
        assert_eq!(name, "review-pr-2");
    }

    // ---- resolve_skill_slash method ----

    use houyicoder_api::skill::{SkillDescriptor, SkillRegistry};
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_resilience::Retry;
    use houyicoder_session::SessionStore;
    use std::sync::Arc;

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
            .resolve_skill_slash(SessionId::new(), "/commit fix typo")
            .await;
        match outcome {
            SkillSlashOutcome::Prepared { name, body } => {
                assert_eq!(name, "commit", "name carried: {name}");
                assert!(body.contains("commit body: fix typo"), "{body}")
            }
            other => panic!("expected Prepared, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_resolve_unknown() {
        let runner = runner_with_slash();
        let outcome = runner.resolve_skill_slash(SessionId::new(), "/nope").await;
        assert!(
            matches!(outcome, SkillSlashOutcome::NotASkill),
            "an unknown /-token is not a skill (falls through)"
        );
    }

    #[tokio::test]
    async fn test_resolve_refused_returns_notice() {
        let runner = runner_with_slash();
        let outcome = runner
            .resolve_skill_slash(SessionId::new(), "/secret")
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
            .resolve_skill_slash(SessionId::new(), "/commit")
            .await;
        assert!(
            matches!(outcome, SkillSlashOutcome::NotASkill),
            "no registry wired -> not a skill"
        );
    }

    /// A full run() with /skill-name lands the raw text as UserInput AND
    /// the prepared body as a MetaUser — the transcript shows what the
    /// user typed, the model reads the body as a directive. Covers the
    /// run() entry integration (resolve + append SkillBody).
    #[tokio::test]
    async fn test_run_lands_skill_body() {
        use houyicoder_context::TurnEventKind;
        let runner = runner_with_slash();
        let session = SessionId::new();
        runner
            .run(session, "/commit fix typo".into())
            .await
            .expect("run completes");
        let view = runner.store().current_view(session).await.unwrap();
        // The raw /-text is the UserInput (what the user typed).
        let user_text = view.events.iter().find_map(|e| match &e.kind {
            TurnEventKind::UserInput { text } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(
            user_text.as_deref(),
            Some("/commit fix typo"),
            "raw /-text kept"
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

    /// A refused skill (/secret, user-invocable=false) ends the turn
    /// without a model call: no MetaUser body appended, no assistant
    /// message, turns=0. The refusal surfaces as a system line (no-op in
    /// tests with no live sink).
    #[tokio::test]
    async fn test_run_refused_skips_model() {
        use houyicoder_context::TurnEventKind;
        let runner = runner_with_slash();
        let session = SessionId::new();
        let result = runner
            .run(session, "/secret".into())
            .await
            .expect("run completes");
        assert_eq!(result.turns, 0, "no model turns for a refused skill");
        assert!(
            matches!(result.outcome, crate::agent::RunOutcome::FinalOutput(_)),
            "turn ended without a model call"
        );
        let view = runner.store().current_view(session).await.unwrap();
        // The raw /-text is kept (the user sees what they typed).
        assert!(view.events.iter().any(|e| matches!(
            e.kind,
            TurnEventKind::UserInput { ref text } if text == "/secret"
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
}
