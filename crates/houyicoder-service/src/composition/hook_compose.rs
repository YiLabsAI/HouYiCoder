//! Builds configured, skill-authored, and built-in hooks for a runner session.
//! Source attribution remains attached so runtime trust policy can filter them.

use super::*;

use houyicoder_api::launcher::ProcessLauncher;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::skill::SkillRegistry;
use houyicoder_api::skill::grant::SkillGrantStore;
use houyicoder_config::settings_merge::read_settings_value;
use houyicoder_config::{
    HookSpec, parse_hooks_from_settings, resolve_hook_policy_settings, resolve_hooks, settings_path,
};
use houyicoder_core::agent::{
    ConditionalActivation, ConditionalSkillActivator, Hook, HookContext, HookError, HookEvent,
    HookPayload, HookVerdict, SkillHookRegistrar, SkillReloadGuard, SkillTool, ToolRegistry,
};
use houyicoder_permission::{Effect, ModeGate, Rule, RuleContent, Scope};
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::reloader::SkillReloader;
use super::skill::SkillRegistryImpl;

/// Command-hook identity across configuration sources. Shell is excluded
/// because program and args already determine the spawned process; including
/// it would distinguish equivalent default-shell declarations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HookDedupKey {
    program: String,
    args: Vec<String>,
    if_condition: Option<String>,
    events: Vec<String>,
    matcher: Option<String>,
}

/// Build command hooks from source-tagged configuration. Unknown events are
/// reported and skipped; source tags preserve the workspace trust boundary.
/// Returns None when no valid hook remains.
pub(crate) fn build_hook_registry(
    specs: &[(HookSpec, HookSource)],
    launcher: Arc<dyn ProcessLauncher>,
    policy: HookPolicy,
) -> Option<Arc<HookRegistry>> {
    if specs.is_empty() {
        return None;
    }
    let registry = Arc::new(HookRegistry::with_policy(policy));
    // Later sources replace equivalent earlier hooks. Indices preserve source
    // order when the winners are registered.
    let mut dedup: HashMap<HookDedupKey, usize> = HashMap::new();
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
                hook = hook.with_timeout(Duration::from_secs(secs));
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

/// Build the shared session registry from configured hooks and the built-in
/// skill-grant hook. Source order defines duplicate precedence, while source
/// tags preserve workspace trust checks.
pub(crate) fn build_session_registry(
    gate: Arc<dyn ModeGate>,
    launcher: Arc<dyn ProcessLauncher>,
    workspace: Option<&Path>,
) -> Arc<HookRegistry> {
    let mut all_specs: Vec<(HookSpec, HookSource)> = Vec::new();
    // Env-var hooks: user source (the user set them in their own shell).
    for spec in resolve_hooks() {
        all_specs.push((spec, HookSource::User));
    }
    // User settings hooks: user source.
    let user_settings = read_settings_value(&settings_path());
    for spec in parse_hooks_from_settings(&user_settings) {
        all_specs.push((spec, HookSource::User));
    }
    // Project and local settings hooks: project and local sources.
    if let Some(ws) = workspace {
        let project_path = ws.join(".houyicoder").join("settings.json");
        let local_path = ws.join(".houyicoder").join("settings.local.json");
        let project_value = read_settings_value(&project_path);
        let local_value = read_settings_value(&local_path);
        for spec in parse_hooks_from_settings(&project_value) {
            all_specs.push((spec, HookSource::Project));
        }
        for spec in parse_hooks_from_settings(&local_value) {
            all_specs.push((spec, HookSource::Local));
        }
    }
    // Only user settings control hook policy. Repository settings cannot
    // weaken or disable the user's hook posture.
    let policy_settings = resolve_hook_policy_settings(&user_settings);
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
    registry: Arc<dyn SkillRegistry>,
    workspace: Option<&Path>,
) -> Arc<dyn ConditionalSkillActivator> {
    Arc::new(ConditionalActivation::new(
        registry,
        workspace
            .map(PathBuf::from)
            .unwrap_or_else(|| env::current_dir().unwrap_or_default()),
    ))
}

/// Discover the skill registry + build its conditional activator in one
/// step; the composition root registers the SkillTool separately. Returns
/// the concrete registry so the hot-reload driver can call reload on it;
/// callers that need the engine-facing port coerce to Arc<dyn SkillRegistry>.
pub(super) fn build_skill_registry_and_activator(
    workspace: Option<&Path>,
) -> (Arc<SkillRegistryImpl>, Arc<dyn ConditionalSkillActivator>) {
    let registry = Arc::new(SkillRegistryImpl::discover(workspace));
    let activator =
        build_conditional_activator(Arc::clone(&registry) as Arc<dyn SkillRegistry>, workspace);
    (registry, activator)
}

/// Build the user grant store. Missing home state disables persistence
/// rather than placing an authority file in the workspace.
pub(super) fn build_skill_grants() -> Option<Arc<SkillGrantStore>> {
    match SkillGrantStore::new() {
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
    tools: &mut ToolRegistry,
    registry: &Arc<SkillRegistryImpl>,
    registrar: &Arc<SkillHookRegistrar>,
    conditional: &Arc<dyn ConditionalSkillActivator>,
    sandbox: Option<Arc<dyn SandboxSession>>,
    skill_grants: Option<Arc<SkillGrantStore>>,
    active_skill: Arc<Mutex<Option<String>>>,
) {
    tools.register(Arc::new(
        SkillTool::new(Arc::clone(registry) as Arc<dyn SkillRegistry>)
            .with_registrar(Arc::clone(registrar))
            .with_activator(Some(Arc::clone(conditional)))
            .with_sandbox(sandbox)
            .with_skill_grants(skill_grants)
            .with_active_skill(Some(Arc::clone(&active_skill))),
    ));
}

/// Build the hot-reload driver, returning the guard (None when no roots
/// exist to watch). Kept here so the composition root's build_runner stays
/// under the per-file size gate.
pub(super) fn build_skill_reloader(
    registry: &Arc<SkillRegistryImpl>,
    conditional: &Arc<dyn ConditionalSkillActivator>,
    registrar: &Arc<SkillHookRegistrar>,
    workspace: Option<PathBuf>,
) -> Option<Arc<dyn SkillReloadGuard>> {
    let home = env::var_os("HOME").map(PathBuf::from);
    SkillReloader::start(
        Arc::clone(registry),
        Arc::clone(conditional),
        Arc::clone(registrar),
        workspace,
        home,
    )
}

/// Applies approved skill-authored tool grants for the current session.
/// The SkillTool result carries the host-derived authority decision; missing
/// or untrusted authority fails closed.
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
        // Built-in grant enforcement remains active when external hooks are disabled.
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
        // The producer grants frontmatter authority only to managed and native
        // user-home skills. Missing or malformed authority fails closed.
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
                    // Tool-wide grants can suppress later prompts, so every
                    // accepted posture change is recorded.
                    tracing::info!(
                        skill = %output.get("skill").and_then(|v| v.as_str()).unwrap_or("?"),
                        grant = %spec,
                        "skill granted a session-scoped always-allow for a tool"
                    );
                    let rule = match content {
                        Some(content) => Rule::with_content(&action, content, Effect::Allow),
                        None => Rule::new(&action, Effect::Allow),
                    };
                    if let Ok(rule) = rule {
                        self.gate.add_rule(rule.with_scope(Scope::Session));
                    }
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
