//! Hook registry wiring for the composition root.
//!
//! Split out of the composition module on size grounds, same reasoning as the
//! memory wiring beside it: whole concerns move out so the composition root
//! stays under the per-file gate. Nothing outside the composition root consumes
//! this, so the seam is local.

use super::*;

use houyicoder_api::launcher::ProcessLauncher;
use houyicoder_core::agent::{Hook, HookContext, HookError, HookEvent, HookPayload, HookVerdict};
use houyicoder_permission::{Effect, ModeGate, Rule, RuleContent, Scope};
use std::sync::Arc;

/// Dedup key for command hooks. Two hooks with the same program, args,
/// if condition, events, AND matcher are the same hook. The matcher is
/// part of the key so the same command registered for different
/// matchers (e.g. Bash and Edit) are distinct hooks — each fires only
/// when its own matcher passes. Events are part of the key so
/// PreToolUse and PostToolUse are distinct. The shell is deliberately
/// absent: it is already fully encoded in program and args, so keying
/// on it as well would split an omitted shell from an explicitly
/// spelled-out default that spawns the identical process, registering
/// the same hook twice and running the command twice. When the same key
/// appears across sources, the last-seen source wins (project overrides
/// user, local overrides project), matching the merge semantics of the
/// reference implementation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HookDedupKey {
    program: String,
    args: Vec<String>,
    if_condition: Option<String>,
    events: Vec<String>,
    matcher: Option<String>,
}

/// Build a hook registry from resolved specs tagged with their source.
/// Each spec's event-name strings parse against the runtime event enum at
/// this composition site (the config crate stays a serde-only leaf with no
/// agent-layer dependency). A spec with any unknown event is skipped with a
/// warning naming the spec and the bad name, so a typo is visible without
/// bricking the run. The matcher, if_condition, and timeout fields on each
/// spec are wired into the CommandHook so the hook filters before spawn and
/// respects its own deadline. Returns None when no spec yields a registered
/// hook, so the caller skips the with_hooks step entirely and the runner
/// runs unchanged.
///
/// Source attribution: the env var a user sets in their own shell is a
/// user-level source, so hooks built from env specs are tagged User
/// (trusted). Hooks parsed from a project or local settings file are tagged
/// Project or Local, so the existing trust gate (which skips Project and
/// Local hooks under an Untrusted workspace) applies to them. This is why
/// the source tag is a parameter here, not a hard-coded value.
pub(crate) fn build_hook_registry(
    specs: &[(houyicoder_config::HookSpec, HookSource)],
    launcher: Arc<dyn ProcessLauncher>,
    policy: HookPolicy,
) -> Option<Arc<HookRegistry>> {
    if specs.is_empty() {
        return None;
    }
    let registry = Arc::new(HookRegistry::with_policy(policy));
    // Last-wins dedup: a later source (project over user, local over
    // project) with the same key replaces the earlier entry. The map
    // stores the index into specs so we can resolve the winner after
    // the single pass.
    let mut dedup: std::collections::HashMap<HookDedupKey, usize> =
        std::collections::HashMap::new();
    for (i, (spec, _source)) in specs.iter().enumerate() {
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
        let dedup_key = HookDedupKey {
            program: spec.program.clone(),
            args: spec.args.clone(),
            if_condition: spec.if_condition.clone(),
            events: spec.events.clone(),
            matcher: spec.matcher.clone(),
        };
        if let Some(prev) = dedup.get(&dedup_key) {
            tracing::debug!(
                hook = %spec.name,
                prev_index = *prev,
                "duplicate hook overrides earlier entry (last-wins)"
            );
        }
        dedup.insert(dedup_key, i);
    }
    // Register the winners in spec order so registration order follows
    // the original source priority (env, user, project, local).
    let mut winner_indices: Vec<usize> = dedup.into_values().collect();
    winner_indices.sort_unstable();
    for i in winner_indices {
        let (spec, source) = &specs[i];
        let mut events = Vec::with_capacity(spec.events.len());
        for ev in &spec.events {
            if let Some(e) = parse_event(ev) {
                events.push(e);
            }
        }
        let mut hook = CommandHook::new(
            spec.name.clone(),
            events,
            spec.program.clone(),
            spec.args.clone(),
            launcher.clone(),
            source.clone(),
        );
        if let Some(m) = &spec.matcher {
            hook = hook.with_matcher(m.clone());
        }
        if let Some(cond) = &spec.if_condition {
            hook = hook.with_if_condition(cond.clone());
        }
        if let Some(secs) = spec.timeout_secs {
            // A timeout of 0 means "no per-hook timeout" (use the
            // registry default), not "timeout immediately". A user
            // setting 0 likely means "disable the per-hook override".
            if secs > 0 {
                hook = hook.with_timeout(std::time::Duration::from_secs(secs));
            }
        }
        if spec.once {
            hook = hook.with_once().with_registry(Arc::clone(&registry));
        }
        let hook = Arc::new(hook);
        let id = registry.register(hook.clone());
        if hook.once() {
            hook.bind_hook_id(id);
        }
    }
    if registry.is_empty() {
        None
    } else {
        Some(registry)
    }
}

/// Build the session hook registry: the external command hooks (from env
/// and from per-source settings files) plus the always-on skill-grant hook.
/// Returns an Arc so the caller can share it between the runner fire points
/// and the skill-hook registrar. An empty spec set still yields an empty
/// registry so the fire points have a home. The launcher is shared with the
/// caller so the skill-hook registrar spawns through the same chokepoint.
///
/// Hooks are gathered per source: env-var hooks are User (trusted, the user
/// set them in their own shell); user settings are User; project settings
/// are Project; local settings are Local. The per-source tag lets the
/// existing trust gate skip Project and Local hooks under an Untrusted
/// workspace. Duplicate hooks (same program, args, shell, if condition,
/// events, and matcher) are deduplicated, keeping the last-seen source
/// (local overrides project overrides user overrides env), matching the
/// merge semantics of the reference implementation.
pub(crate) fn build_session_registry(
    gate: Arc<dyn ModeGate>,
    launcher: Arc<dyn ProcessLauncher>,
    workspace: Option<&std::path::Path>,
) -> Arc<HookRegistry> {
    let mut all_specs: Vec<(houyicoder_config::HookSpec, HookSource)> = Vec::new();
    // Env-var hooks: user source (the user set them in their own shell).
    for spec in houyicoder_config::resolve_hooks() {
        all_specs.push((spec, HookSource::User));
    }
    // User settings hooks: user source.
    let user_settings =
        houyicoder_config::settings_merge::read_settings_value(&houyicoder_config::settings_path());
    for spec in houyicoder_config::parse_hooks_from_settings(&user_settings) {
        all_specs.push((spec, HookSource::User));
    }
    // Project and local settings hooks: project and local sources.
    if let Some(ws) = workspace {
        let project_path = ws.join(".houyicoder").join("settings.json");
        let local_path = ws.join(".houyicoder").join("settings.local.json");
        let project_value = houyicoder_config::settings_merge::read_settings_value(&project_path);
        let local_value = houyicoder_config::settings_merge::read_settings_value(&local_path);
        for spec in houyicoder_config::parse_hooks_from_settings(&project_value) {
            all_specs.push((spec, HookSource::Project));
        }
        for spec in houyicoder_config::parse_hooks_from_settings(&local_value) {
            all_specs.push((spec, HookSource::Local));
        }
    }
    // Resolve hook policy from USER settings only. These are managed
    // settings (admin/MDM scope), not project-local overrides: a cloned
    // repository must not disable the user's hooks or force managed-only
    // by writing these fields into a project settings file. Reading
    // from the merged value would let a project file override the user's
    // security posture.
    let policy_settings = houyicoder_config::resolve_hook_policy_settings(&user_settings);
    let policy = if policy_settings.allow_managed_hooks_only {
        HookPolicy::ManagedOnly
    } else if policy_settings.disable_all_hooks {
        HookPolicy::NonManagedDisabled
    } else {
        HookPolicy::AllEnabled
    };
    let reg: Arc<HookRegistry> = match build_hook_registry(&all_specs, launcher, policy.clone()) {
        Some(r) => r,
        None => Arc::new(HookRegistry::with_policy(policy)),
    };
    reg.register(Arc::new(SkillGrantHook::new(gate)));
    reg
}

/// Build the session-scoped paths-skill activator. One Arc shared by the
/// Runner and the file-touch tools so the active set is per-session.
pub(super) fn build_conditional_activator(
    registry: Arc<dyn houyicoder_api::skill::SkillRegistry>,
    workspace: Option<&std::path::Path>,
) -> Arc<dyn houyicoder_core::agent::ConditionalSkillActivator> {
    Arc::new(houyicoder_core::agent::ConditionalActivation::new(
        registry,
        workspace
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
    ))
}

/// Discover the skill registry + build its conditional activator in one
/// step; the composition root registers the SkillTool separately. Returns
/// the concrete registry so the hot-reload driver can call reload on it;
/// callers that need the engine-facing port coerce to Arc<dyn SkillRegistry>.
pub(super) fn build_skill_registry_and_activator(
    workspace: Option<&std::path::Path>,
) -> (
    Arc<super::skill::SkillRegistryImpl>,
    Arc<dyn houyicoder_core::agent::ConditionalSkillActivator>,
) {
    let registry = Arc::new(super::skill::SkillRegistryImpl::discover(workspace));
    let activator = build_conditional_activator(
        std::sync::Arc::clone(&registry) as Arc<dyn houyicoder_api::skill::SkillRegistry>,
        workspace,
    );
    (registry, activator)
}

/// Build the user grant store. Missing home state disables persistence
/// rather than placing an authority file in the workspace.
pub(super) fn build_skill_grants() -> Option<Arc<houyicoder_api::skill::grant::SkillGrantStore>> {
    match houyicoder_api::skill::grant::SkillGrantStore::new() {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            tracing::warn!("skill grant store unavailable: {e}");
            None
        }
    }
}

/// Register the SkillTool: resolves skill names through the registry, gates
/// invocation through the registrar + conditional activator. Not
/// sandbox-backed (reads skill files directly), so registered directly.
pub(super) fn register_skill_tool(
    tools: &mut houyicoder_core::agent::ToolRegistry,
    registry: &Arc<super::skill::SkillRegistryImpl>,
    registrar: &Arc<houyicoder_core::agent::SkillHookRegistrar>,
    conditional: &Arc<dyn houyicoder_core::agent::ConditionalSkillActivator>,
    sandbox: Option<Arc<dyn houyicoder_api::sandbox::SandboxSession>>,
    skill_grants: Option<Arc<houyicoder_api::skill::grant::SkillGrantStore>>,
    active_skill: Arc<std::sync::Mutex<Option<String>>>,
) {
    tools.register(Arc::new(
        houyicoder_core::agent::SkillTool::new(
            std::sync::Arc::clone(registry) as Arc<dyn houyicoder_api::skill::SkillRegistry>
        )
        .with_registrar(std::sync::Arc::clone(registrar))
        .with_activator(Some(std::sync::Arc::clone(conditional)))
        .with_sandbox(sandbox)
        .with_skill_grants(skill_grants)
        .with_active_skill(Some(std::sync::Arc::clone(&active_skill))),
    ));
}

/// Build the hot-reload driver, returning the guard (None when no roots
/// exist to watch). Kept here so the composition root's build_runner stays
/// under the per-file size gate.
pub(super) fn build_skill_reloader(
    registry: &Arc<super::skill::SkillRegistryImpl>,
    conditional: &Arc<dyn houyicoder_core::agent::ConditionalSkillActivator>,
    registrar: &Arc<houyicoder_core::agent::SkillHookRegistrar>,
    workspace: Option<std::path::PathBuf>,
) -> Option<Arc<dyn houyicoder_core::agent::SkillReloadGuard>> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    super::reloader::SkillReloader::start(
        std::sync::Arc::clone(registry),
        std::sync::Arc::clone(conditional),
        std::sync::Arc::clone(registrar),
        workspace,
        home,
    )
}

/// A built-in PostToolUse hook that reads the SkillTool result for
/// allowed_tools and adds them as session-scoped Allow rules. This is
/// the additive grant: after a skill with allowed_tools is invoked
/// (and approved via the safe-property allowlist), its tool grants
/// become session-scoped always-allow so the granted tools do not
/// re-ask during the skill execution. The grant is non-persistent
/// (Scope::Session, cleared on restart) and has no end event — it
/// decays with the session lifetime. Source-gated: only a managed or
/// user source (the SkillTool result carries trusted=true) may install
/// the grant; a project, ecosystem, or local source's tools re-ask on
/// each call so a skill the user never vetted cannot pre-authorize its
/// tools.
pub(crate) struct SkillGrantHook {
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
        // Managed: this is a built-in engine hook, not a user/project
        // hook. A Managed source survives the ManagedOnly and
        // NonManagedDisabled policy gates so the skill-grant path stays
        // active even when the user disables external hooks.
        HookSource::Managed
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
        // Gate the additive grant by the skill's discovery source: only a
        // managed or user source is trusted enough for a session-scoped
        // always-allow on its tools. A project, ecosystem, or local source
        // would otherwise install a durable blanket allow for tools the user
        // never vetted — its tools re-ask on each call instead. Fail closed
        // when the trust flag is absent (a result shape the engine did not
        // produce) so a non-standard result grants nothing.
        let trusted = output
            .get("trusted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !trusted {
            return Ok(HookVerdict::Allow);
        }
        if let Some(tools) = output.get("allowed_tools").and_then(|v| v.as_array()) {
            for tool in tools {
                if let Some(spec) = tool.as_str() {
                    let (action, content) = parse_tool_grant(spec);
                    // A session-scoped Allow widens the security posture for
                    // the rest of the session (a bare name like "Bash" closes
                    // the bash gate entirely), so surface it in the tracing
                    // span — a silent posture change is the failure mode this
                    // gate exists to prevent.
                    tracing::info!(
                        skill = %output.get("skill").and_then(|v| v.as_str()).unwrap_or("?"),
                        grant = %spec,
                        "skill granted a session-scoped always-allow for a tool"
                    );
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
#[path = "hook_compose_tests.rs"]
mod hook_compose_tests;
