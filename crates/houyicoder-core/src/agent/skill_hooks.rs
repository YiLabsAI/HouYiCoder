//! Skill frontmatter hooks: the session-scoped registration path. A skill's
//! parsed hooks become command hooks in the session HookRegistry when the
//! skill is invoked, not at load. A dedup set makes a re-invoke a no-op, and
//! a live workspace-trust ref fail-closes Project and Local sources before
//! registration under an Untrusted workspace.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

use houyicoder_api::skill::{HookSourceKind, SkillRegistry};
use houyicoder_api::trust::TrustState;

use super::exports::{
    Hook, HookContext, HookError, HookEvent, HookRegistry, HookSource, HookVerdict,
};
use super::parse_event;

/// Shared registration state for both skill-invocation paths: the session
/// hook registry, a live trust ref the server writes after the startup
/// trust prompt, and a dedup set so a re-invoke does not register a second
/// firing copy.
pub struct SkillHookRegistrar {
    hook_reg: Arc<HookRegistry>,
    trust: Arc<RwLock<TrustState>>,
    seen: Mutex<HashSet<DedupKey>>,
}

/// Identity a registered skill hook is deduped by. Two specs that agree on
/// skill, event, matcher, command, and args are the same hook. Args are part
/// of the key so two hooks that differ only by arguments both register.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DedupKey {
    skill: String,
    event: String,
    matcher: Option<String>,
    command: String,
    args: Vec<String>,
}

impl SkillHookRegistrar {
    /// Build from the session hook registry and a live trust ref. The trust
    /// ref starts fail-closed (Untrusted); the server writes the resolved
    /// state back so a Project or Local source invoked later reads it.
    pub fn new(hook_reg: Arc<HookRegistry>, trust: Arc<RwLock<TrustState>>) -> Self {
        Self {
            hook_reg,
            trust,
            seen: Mutex::new(HashSet::new()),
        }
    }

    /// Write the resolved workspace trust. The server calls this once after
    /// the startup trust prompt. Idempotent.
    pub fn set_trust(&self, state: TrustState) {
        let mut t = self.trust.write().unwrap_or_else(|e| e.into_inner());
        *t = state;
    }

    /// Register every parsed hook for the skill as a session-scoped command
    /// hook. Returns the count newly registered. A Project or Local source
    /// under an Untrusted workspace is skipped fail-closed; an unknown event
    /// name is skipped with a warning; a spec already in the seen set is
    /// skipped (the re-invoke no-op). Managed and User sources pass.
    pub fn register(&self, registry: &dyn SkillRegistry, skill_name: &str) -> usize {
        let specs = registry.hooks_for(skill_name);
        if specs.is_empty() {
            return 0;
        }
        let trust_now = self.trust.read().unwrap_or_else(|e| e.into_inner()).clone();
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let mut count = 0;
        for spec in specs {
            let gated = matches!(spec.source, HookSourceKind::Project | HookSourceKind::Local);
            if gated && trust_now == TrustState::Untrusted {
                tracing::info!(
                    skill = %skill_name,
                    event = %spec.event,
                    source = ?spec.source,
                    "skill hook skipped: untrusted workspace blocks project/local source"
                );
                continue;
            }
            let Some(event) = parse_event(&spec.event) else {
                tracing::warn!(
                    skill = %skill_name,
                    event = %spec.event,
                    "skill hook skipped: unknown event name"
                );
                continue;
            };
            let key = DedupKey {
                skill: skill_name.to_string(),
                event: spec.event.clone(),
                matcher: spec.matcher.clone(),
                command: spec.command.clone(),
                args: spec.args.clone(),
            };
            if !seen.insert(key.clone()) {
                tracing::info!(
                    skill = %skill_name,
                    event = %spec.event,
                    "skill hook skipped: already registered this session (dedup)"
                );
                continue;
            }
            let source = map_source(spec.source);
            let hook = SkillCommandHook::new(skill_name.to_string(), vec![event], source);
            self.hook_reg.register(Arc::new(hook));
            count += 1;
            tracing::info!(
                skill = %skill_name,
                event = %spec.event,
                source = ?spec.source,
                trust = ?trust_now,
                "registered skill hook"
            );
        }
        count
    }
}

fn map_source(kind: HookSourceKind) -> HookSource {
    match kind {
        HookSourceKind::Managed => HookSource::Managed,
        HookSourceKind::User => HookSource::User,
        HookSourceKind::Project => HookSource::Project,
        HookSourceKind::Local => HookSource::Local,
    }
}

/// A hook built from a skill frontmatter spec. Carries the skill name for
/// diagnostics, the subscribed event, and the source level for the registry
/// policy and trust filters at dispatch. The verdict is Allow: this stage
/// wires registration, trust gating, dedup, and dispatch visibility.
pub(crate) struct SkillCommandHook {
    name: String,
    events: Vec<HookEvent>,
    source: HookSource,
}

impl SkillCommandHook {
    pub(crate) fn new(name: String, events: Vec<HookEvent>, source: HookSource) -> Self {
        Self {
            name,
            events,
            source,
        }
    }
}

impl Hook for SkillCommandHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn events(&self) -> &[HookEvent] {
        &self.events
    }
    fn source(&self) -> HookSource {
        self.source.clone()
    }
    fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
        Ok(HookVerdict::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_api::skill::{SkillDescriptor, SkillError, SkillHookSpec, SkillRegistry};
    use houyicoder_context::SessionId;
    use std::sync::RwLock;

    use super::super::{HookPayload, ToolResult};

    /// A stub registry that returns a fixed vec of specs from hooks_for. find
    /// and prepare_body are not exercised by the registrar.
    struct SpecRegistry {
        specs: Vec<SkillHookSpec>,
    }
    impl SkillRegistry for SpecRegistry {
        fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
            Vec::new()
        }
        fn find(&self, _name: &str) -> Option<SkillDescriptor> {
            None
        }
        fn prepare_body(
            &self,
            name: &str,
            _args: Option<&str>,
            _sid: Option<&str>,
        ) -> Result<String, SkillError> {
            Err(SkillError::NotFound(name.into()))
        }
        fn hooks_for(&self, _name: &str) -> Vec<SkillHookSpec> {
            self.specs.clone()
        }
    }

    fn spec(event: &str, source: HookSourceKind) -> SkillHookSpec {
        SkillHookSpec {
            event: event.into(),
            matcher: None,
            command: "echo".into(),
            args: vec![],
            once: false,
            if_rule: None,
            source,
        }
    }

    fn registrar(trust: TrustState) -> (SkillHookRegistrar, Arc<HookRegistry>) {
        let reg = Arc::new(HookRegistry::new());
        let trust = Arc::new(RwLock::new(trust));
        let r = SkillHookRegistrar::new(reg.clone(), trust);
        (r, reg)
    }

    fn post_tool_ctx() -> HookContext {
        HookContext {
            event: HookEvent::PostToolUse,
            payload: HookPayload::PostToolUse {
                tool_name: "bash".into(),
                input: serde_json::json!({}),
                result: ToolResult {
                    output: "{}".into(),
                },
            },
            session: SessionId::new(),
        }
    }

    /// A registered Managed hook fires on dispatch (an Allow outcome).
    #[test]
    fn test_register_fires_on_dispatch() {
        let (r, reg) = registrar(TrustState::Trusted);
        let registry = SpecRegistry {
            specs: vec![spec("PostToolUse", HookSourceKind::Managed)],
        };
        assert_eq!(r.register(&registry, "deploy"), 1);
        let outcomes = reg.dispatch(&post_tool_ctx());
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].result, Ok(HookVerdict::Allow)));
    }

    /// An empty specs vec registers nothing (the shape a Mcp source
    /// produces: parse filtered it to empty).
    #[test]
    fn test_empty_specs_registers_nothing() {
        let (r, reg) = registrar(TrustState::Trusted);
        let registry = SpecRegistry { specs: vec![] };
        assert_eq!(r.register(&registry, "deploy"), 0);
        assert!(reg.is_empty());
    }

    /// A second register call for the same spec is a no-op: the dedup key is
    /// already in the seen set, so dispatch still fires once.
    #[test]
    fn test_dedup_skips_duplicate() {
        let (r, reg) = registrar(TrustState::Trusted);
        let registry = SpecRegistry {
            specs: vec![spec("PostToolUse", HookSourceKind::Managed)],
        };
        assert_eq!(r.register(&registry, "deploy"), 1);
        assert_eq!(r.register(&registry, "deploy"), 0, "second call is no-op");
        let outcomes = reg.dispatch(&post_tool_ctx());
        assert_eq!(outcomes.len(), 1, "one hook, not two");
    }

    /// Two specs that differ only by args both register: args are part of the
    /// dedup key, so two argument sets on the same command are distinct hooks.
    #[test]
    fn test_args_differ_both_register() {
        let (r, reg) = registrar(TrustState::Trusted);
        let mut a = spec("PostToolUse", HookSourceKind::Managed);
        a.args = vec!["lint".into()];
        let mut b = spec("PostToolUse", HookSourceKind::Managed);
        b.args = vec!["test".into()];
        let registry = SpecRegistry { specs: vec![a, b] };
        assert_eq!(r.register(&registry, "deploy"), 2);
        assert_eq!(reg.dispatch(&post_tool_ctx()).len(), 2);
    }

    /// A Project source under an Untrusted workspace is skipped before
    /// registration: the hook never enters the registry, so dispatch cannot
    /// fire it.
    #[test]
    fn test_project_untrusted_skipped() {
        let (r, reg) = registrar(TrustState::Untrusted);
        let registry = SpecRegistry {
            specs: vec![spec("PostToolUse", HookSourceKind::Project)],
        };
        assert_eq!(r.register(&registry, "deploy"), 0);
        assert!(reg.is_empty(), "no hook registered for untrusted project");
    }

    /// A Project source under a Trusted or Acknowledged workspace registers.
    #[test]
    fn test_project_trusted_registers() {
        let (r, reg) = registrar(TrustState::Acknowledged);
        let registry = SpecRegistry {
            specs: vec![spec("PostToolUse", HookSourceKind::Project)],
        };
        assert_eq!(r.register(&registry, "deploy"), 1);
        assert_eq!(reg.dispatch(&post_tool_ctx()).len(), 1);
    }

    /// A User source passes the trust gate even when the workspace is
    /// Untrusted: a user-level hook lives on the user's machine, not in the
    /// repository, so the clone-and-open threat the gate defends against does
    /// not arise.
    #[test]
    fn test_user_source_passes_untrusted() {
        let (r, reg) = registrar(TrustState::Untrusted);
        let registry = SpecRegistry {
            specs: vec![spec("PostToolUse", HookSourceKind::User)],
        };
        assert_eq!(r.register(&registry, "deploy"), 1);
        assert_eq!(reg.dispatch(&post_tool_ctx()).len(), 1);
    }

    /// An unknown event name is skipped with a warning; the rest of the specs
    /// still register.
    #[test]
    fn test_unknown_event_skipped() {
        let (r, reg) = registrar(TrustState::Trusted);
        let registry = SpecRegistry {
            specs: vec![
                spec("BogusEvent", HookSourceKind::Managed),
                spec("PostToolUse", HookSourceKind::Managed),
            ],
        };
        assert_eq!(r.register(&registry, "deploy"), 1, "only the known event");
        assert_eq!(reg.dispatch(&post_tool_ctx()).len(), 1);
    }

    /// set_trust is read live, not snapshotted: a registrar built Untrusted
    /// skips a Project spec on the first call, but after set_trust the same
    /// spec registers.
    #[test]
    fn test_set_trust_liveness() {
        let (r, reg) = registrar(TrustState::Untrusted);
        let registry = SpecRegistry {
            specs: vec![spec("PostToolUse", HookSourceKind::Project)],
        };
        assert_eq!(r.register(&registry, "deploy"), 0, "untrusted skips");
        r.set_trust(TrustState::Acknowledged);
        assert_eq!(
            r.register(&registry, "deploy"),
            1,
            "after trust resolves, the spec registers"
        );
        assert_eq!(reg.dispatch(&post_tool_ctx()).len(), 1);
    }
}
