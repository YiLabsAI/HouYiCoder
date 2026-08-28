//! Hook registry wiring for the composition root.
//!
//! Split out of the composition module on size grounds, same reasoning as the
//! memory wiring beside it: whole concerns move out so the composition root
//! stays under the per-file gate. Nothing outside the composition root consumes
//! this, so the seam is local.

use super::*;

use houyicoder_core::agent::{Hook, HookContext, HookError, HookEvent, HookPayload, HookVerdict};
use houyicoder_permission::{Effect, ModeGate, Rule, RuleContent, Scope};
use std::sync::Arc;

/// Build a hook registry from resolved specs. Each spec's event-name strings
/// parse against the runtime event enum at this composition site (the config
/// crate stays a serde-only leaf with no agent-layer dependency). A spec with
/// any unknown event is skipped with a stderr warning naming the spec and the
/// bad name, so a typo is visible without bricking the run. Returns None when
/// no spec yields a registered hook, so the caller skips the with_hooks step
/// entirely and the runner runs unchanged.
///
/// Source attribution: the env var a user sets in their own shell is a
/// user-level source, so hooks built here are tagged User (trusted). This
/// matters for the trust filter that lands later: a user-set env var cannot
/// travel with a cloned repository, so the clone-and-open threat model that
/// trust prompt defends against does not arise for this source. A future
/// project-level hook source read from a repository file MUST gate execution
/// on the trust prompt before any spawn; that is why the source tag is a
/// parameter here, not a hard-coded Project.
pub(super) fn build_hook_registry(
    specs: &[houyicoder_config::HookSpec],
    launcher: Arc<dyn ProcessLauncher>,
) -> Option<HookRegistry> {
    if specs.is_empty() {
        return None;
    }
    let registry = HookRegistry::new();
    for spec in specs {
        let mut events = Vec::with_capacity(spec.events.len());
        let mut bad = None;
        for ev in &spec.events {
            match parse_event(ev) {
                Some(e) => events.push(e),
                None => {
                    bad = Some(ev.as_str());
                    break;
                }
            }
        }
        if let Some(name) = bad {
            tracing::warn!("hook {:?} ignored: unknown event {name:?}", spec.name);
            continue;
        }
        if events.is_empty() {
            continue;
        }
        let hook = CommandHook::new(
            spec.name.clone(),
            events,
            spec.program.clone(),
            spec.args.clone(),
            launcher.clone(),
            HookSource::User,
        );
        registry.register(Arc::new(hook));
    }
    if registry.is_empty() {
        None
    } else {
        Some(registry)
    }
}

/// A built-in PostToolUse hook that reads the SkillTool result for
/// allowed_tools and adds them as session-scoped Allow rules. This is
/// the additive grant: after a skill with allowed_tools is invoked
/// (and approved via the safe-property allowlist), its tool grants
/// become session-scoped always-allow so the granted tools do not
/// re-ask during the skill execution. The grant is non-persistent
/// (Scope::Session, cleared on restart) and has no end event — it
/// decays with the session lifetime.
pub(super) struct SkillGrantHook {
    gate: Arc<dyn ModeGate>,
}

impl SkillGrantHook {
    pub(super) fn new(gate: Arc<dyn ModeGate>) -> Self {
        Self { gate }
    }
}

impl Hook for SkillGrantHook {
    fn name(&self) -> &str {
        "skill-grant"
    }
    fn events(&self) -> &[HookEvent] {
        &[HookEvent::PostToolUse]
    }
    fn source(&self) -> HookSource {
        HookSource::User
    }
    fn evaluate(&self, ctx: &HookContext) -> Result<HookVerdict, HookError> {
        if ctx.event != HookEvent::PostToolUse {
            return Ok(HookVerdict::Allow);
        }
        let result = match &ctx.payload {
            HookPayload::PostToolUse {
                tool_name, result, ..
            } if tool_name == "skill" => result,
            _ => return Ok(HookVerdict::Allow),
        };
        let output: serde_json::Value = serde_json::from_str(&result.output).unwrap_or_default();
        if let Some(tools) = output.get("allowed_tools").and_then(|v| v.as_array()) {
            for tool in tools {
                if let Some(spec) = tool.as_str() {
                    let (action, content) = parse_tool_grant(spec);
                    self.gate.add_rule(Rule {
                        action,
                        content,
                        effect: Effect::Allow,
                        scope: Scope::Session,
                    });
                }
            }
        }
        Ok(HookVerdict::Allow)
    }
}

/// Parse an allowed-tools entry into a tool name + optional content
/// pattern. A plain name like "Bash" yields (action, None). A scoped
/// form like "Bash(git *)" yields ("Bash", Some(Glob("git *"))).
fn parse_tool_grant(spec: &str) -> (String, Option<RuleContent>) {
    if let Some(open) = spec.find('(') {
        let action = spec[..open].to_string();
        let inner = spec[open + 1..].trim_end_matches(')');
        (action, Some(RuleContent::parse(inner)))
    } else {
        (spec.to_string(), None)
    }
}

#[cfg(test)]
mod hook_tests {
    use super::*;
    use houyicoder_config::HookSpec;
    use houyicoder_core::agent::ToolResult;

    fn launcher() -> Arc<dyn ProcessLauncher> {
        Arc::new(houyicoder_api::launcher::StdProcessLauncher::new())
    }

    fn spec(name: &str, events: &[&str], program: &str) -> HookSpec {
        HookSpec {
            name: name.into(),
            events: events.iter().map(|s| (*s).into()).collect(),
            program: program.into(),
            args: Vec::new(),
        }
    }

    #[test]
    fn test_build_hooks_empty() {
        assert!(build_hook_registry(&[], launcher()).is_none());
    }

    #[test]
    fn test_build_hooks_valid() {
        let specs = vec![spec("lint", &["PreToolUse"], "true")];
        let reg = build_hook_registry(&specs, launcher()).expect("one valid hook");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn test_build_hooks_skips_bad() {
        // A spec with one known and one unknown event is skipped entirely;
        // a second valid spec still registers.
        let specs = vec![
            spec("bad", &["PreToolUse", "Nope"], "true"),
            spec("ok", &["PostToolUse"], "true"),
        ];
        let reg = build_hook_registry(&specs, launcher()).expect("second spec registers");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn test_build_hooks_all_bad() {
        let specs = vec![spec("bad", &["DefinitelyNotAnEvent"], "true")];
        assert!(build_hook_registry(&specs, launcher()).is_none());
    }

    #[test]
    fn test_build_hooks_no_events() {
        // A spec with an empty events list registers nothing (a hook that
        // fires on no event is a no-op; the typo is surfaced at registration,
        // not at fire time).
        let specs = vec![spec("idle", &[], "true")];
        assert!(build_hook_registry(&specs, launcher()).is_none());
    }

    /// After the Skill tool runs, the grant hook reads allowed_tools
    /// from the result and adds session-scoped Allow rules so the
    /// granted tools do not re-ask during the skill execution.
    #[test]
    fn test_grant_adds_rules() {
        use houyicoder_context::SessionId;
        use houyicoder_permission::DefaultModeGate;

        let gate = Arc::new(DefaultModeGate::new());
        let hook = SkillGrantHook::new(gate.clone());

        let ctx = HookContext {
            event: HookEvent::PostToolUse,
            payload: HookPayload::PostToolUse {
                tool_name: "skill".to_string(),
                input: serde_json::json!({"skill":"commit"}),
                result: ToolResult {
                    output: serde_json::json!({
                        "skill": "commit",
                        "result": "body",
                        "allowed_tools": ["Bash", "Read"]
                    })
                    .to_string(),
                },
            },
            session: SessionId::new(),
        };

        let verdict = hook.evaluate(&ctx).unwrap();
        assert!(matches!(verdict, HookVerdict::Allow), "grant hook allows");

        let rules = gate.rules();
        assert!(
            rules.iter().any(|r| r.action == "Bash"
                && r.effect == Effect::Allow
                && r.scope == Scope::Session),
            "Bash session Allow rule added"
        );
        assert!(
            rules.iter().any(|r| r.action == "Read"
                && r.effect == Effect::Allow
                && r.scope == Scope::Session),
            "Read session Allow rule added"
        );
    }

    /// A non-skill tool does not trigger the grant.
    #[test]
    fn test_grant_skips_other() {
        use houyicoder_context::SessionId;
        use houyicoder_permission::DefaultModeGate;

        let gate = Arc::new(DefaultModeGate::new());
        let hook = SkillGrantHook::new(gate.clone());

        let ctx = HookContext {
            event: HookEvent::PostToolUse,
            payload: HookPayload::PostToolUse {
                tool_name: "bash".to_string(),
                input: serde_json::json!({}),
                result: ToolResult {
                    output: "{}".to_string(),
                },
            },
            session: SessionId::new(),
        };

        let verdict = hook.evaluate(&ctx).unwrap();
        assert!(matches!(verdict, HookVerdict::Allow));
        assert!(
            !gate
                .rules()
                .iter()
                .any(|r| r.scope == Scope::Session && r.effect == Effect::Allow),
            "no session Allow rules added for non-skill tool"
        );
    }
}
