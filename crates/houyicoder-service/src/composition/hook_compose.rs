//! Builds configured, skill-authored, and built-in hooks for a runner session.
//! Source attribution remains attached so runtime trust policy can filter them.

use super::*;

use houyicoder_api::launcher::{ProcessLauncher, StdProcessLauncher};
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
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::fs::{metadata, read_to_string};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

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
    install_configured_hooks(&registry, specs, &launcher, None);
    if registry.is_empty() {
        None
    } else {
        Some(registry)
    }
}

/// Deduplicate and install settings-configured command hooks into the target
/// registry, replacing any earlier configured hooks while keeping built-in and
/// skill hooks in place.
fn install_configured_hooks(
    registry: &Arc<HookRegistry>,
    specs: &[(HookSpec, HookSource)],
    launcher: &Arc<dyn ProcessLauncher>,
    workspace: Option<&Path>,
) {
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
    let mut winner_indices: Vec<usize> = dedup.into_values().collect();
    winner_indices.sort_unstable();
    let mut cmd_hooks: Vec<Arc<CommandHook>> = Vec::with_capacity(winner_indices.len());
    let mut dyn_hooks: Vec<Arc<dyn Hook>> = Vec::with_capacity(winner_indices.len());
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
        if let Some(ws) = workspace {
            hook = hook.with_workspace(ws.to_path_buf());
        }
        if let Some(m) = &spec.matcher {
            hook = hook.with_matcher(m.clone());
        }
        if let Some(cond) = &spec.if_condition {
            hook = hook.with_if_condition(cond.clone());
        }
        if let Some(secs) = spec.timeout_secs
            && secs > 0
        {
            hook = hook.with_timeout(Duration::from_secs(secs));
        }
        if spec.once {
            hook = hook.with_once().with_registry(Arc::clone(registry));
        }
        let hook = Arc::new(hook);
        dyn_hooks.push(Arc::clone(&hook) as Arc<dyn Hook>);
        cmd_hooks.push(hook);
    }
    let ids = registry.replace_configured(&dyn_hooks);
    for (id, hook) in ids.into_iter().zip(cmd_hooks) {
        if hook.once() {
            hook.bind_hook_id(id);
        }
    }
}

/// Resolve the main repository checkout when the given workspace is a linked
/// worktree. A linked worktree has a .git file pointing to
/// <main>/.git/worktrees/<name>; reading it avoids spawning a git subprocess on
/// the hot dispatch path.
fn linked_worktree_main_root(workspace: &Path) -> Option<PathBuf> {
    let dot_git = workspace.join(".git");
    let text = read_to_string(&dot_git).ok()?;
    let raw = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    let gitdir = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        workspace.join(raw)
    };
    let worktrees_dir = gitdir.parent()?;
    if worktrees_dir.file_name()?.to_str()? != "worktrees" {
        return None;
    }
    let common_git = worktrees_dir.parent()?;
    let main_root = common_git.parent()?;
    (main_root != workspace && main_root.is_dir()).then(|| main_root.to_path_buf())
}

/// Expand project-root environment placeholders and rewrite inherited
/// main-checkout paths so a hook command runs against the active worktree.
///
/// The main-checkout rewrite runs first. A linked worktree lives under the
/// main checkout, so its own path starts with the main path; expanding a
/// placeholder first would place the active path there, and the main-to-active
/// rewrite would then match inside it and nest the worktree path in itself.
fn rewrite_hook_spec(
    mut spec: HookSpec,
    active_ws: &Path,
    inherited_from: Option<&Path>,
) -> HookSpec {
    let ws_str = active_ws.to_string_lossy();
    let main_str = inherited_from.map(|p| p.to_string_lossy().into_owned());
    for arg in &mut spec.args {
        // A command that already names the active tree is left alone. The
        // rewrite is only for a path copied from the main checkout, and a
        // second pass over a path that already points here would nest the
        // worktree inside itself.
        if let Some(main) = &main_str
            && !arg.contains(ws_str.as_ref())
        {
            *arg = arg.replace(main, &ws_str);
        }
        *arg = arg
            .replace("${HOUYI_PROJECT_DIR}", &ws_str)
            .replace("$HOUYI_PROJECT_DIR", &ws_str)
            .replace("${CLAUDE_PROJECT_DIR}", &ws_str)
            .replace("$CLAUDE_PROJECT_DIR", &ws_str);
    }
    spec
}

/// Candidate settings files that supply hooks for the given workspace, in
/// precedence order (user, inherited main checkout when in a linked worktree,
/// then the active workspace's project and local settings).
fn candidate_settings_files(workspace: Option<&Path>) -> Vec<PathBuf> {
    let mut files = vec![settings_path()];
    if let Some(ws) = workspace {
        if let Some(main_root) = linked_worktree_main_root(ws) {
            files.push(main_root.join(".claude").join("settings.json"));
            files.push(main_root.join(".houyicoder").join("settings.json"));
        }
        files.push(ws.join(".claude").join("settings.json"));
        files.push(ws.join(".houyicoder").join("settings.json"));
        files.push(ws.join(".claude").join("settings.local.json"));
        files.push(ws.join(".houyicoder").join("settings.local.json"));
    }
    files
}

fn collect_configured_specs(
    user_settings: &Value,
    workspace: Option<&Path>,
) -> Vec<(HookSpec, HookSource)> {
    let mut all_specs: Vec<(HookSpec, HookSource)> = Vec::new();
    for spec in resolve_hooks() {
        all_specs.push((spec, HookSource::User));
    }
    for spec in parse_hooks_from_settings(user_settings) {
        all_specs.push((spec, HookSource::User));
    }
    if let Some(ws) = workspace {
        if let Some(main_root) = linked_worktree_main_root(ws) {
            for dir in [".claude", ".houyicoder"] {
                let path = main_root.join(dir).join("settings.json");
                for spec in parse_hooks_from_settings(&read_settings_value(&path)) {
                    all_specs.push((
                        rewrite_hook_spec(spec, ws, Some(&main_root)),
                        HookSource::Project,
                    ));
                }
            }
        }
        for (dir, file, source) in [
            (".claude", "settings.json", HookSource::Project),
            (".houyicoder", "settings.json", HookSource::Project),
            (".claude", "settings.local.json", HookSource::Local),
            (".houyicoder", "settings.local.json", HookSource::Local),
        ] {
            let path = ws.join(dir).join(file);
            for spec in parse_hooks_from_settings(&read_settings_value(&path)) {
                all_specs.push((rewrite_hook_spec(spec, ws, None), source.clone()));
            }
        }
    }
    all_specs
}

type FileStamp = (PathBuf, Option<(SystemTime, u64)>);

/// Tracks the active working directory and settings file timestamps so
/// configured hooks reload on the spot when entering a worktree or laying down
/// a settings file, without requiring an exit-and-reenter cycle.
pub(super) struct ConfigHookReloader {
    registry: Weak<HookRegistry>,
    launcher: Arc<dyn ProcessLauncher>,
    fallback_cwd: Option<PathBuf>,
    runner_cwd: Mutex<Option<Arc<RwLock<PathBuf>>>>,
    snapshot: Mutex<Option<(Option<PathBuf>, Vec<FileStamp>)>>,
}

impl ConfigHookReloader {
    fn new(
        registry: &Arc<HookRegistry>,
        launcher: Arc<dyn ProcessLauncher>,
        workspace: Option<&Path>,
    ) -> Arc<Self> {
        let ws = workspace.map(PathBuf::from);
        let stamps = Self::read_stamps(ws.as_deref());
        Arc::new(Self {
            registry: Arc::downgrade(registry),
            launcher,
            fallback_cwd: ws.clone(),
            runner_cwd: Mutex::new(None),
            snapshot: Mutex::new(Some((ws, stamps))),
        })
    }

    fn bind_runner_cwd(&self, cwd: Arc<RwLock<PathBuf>>) {
        *self.runner_cwd.lock().unwrap_or_else(|e| e.into_inner()) = Some(cwd);
    }

    fn active_cwd(&self) -> Option<PathBuf> {
        if let Some(handle) = self
            .runner_cwd
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Some(handle.read().unwrap_or_else(|e| e.into_inner()).clone());
        }
        self.fallback_cwd.clone()
    }

    fn read_stamps(workspace: Option<&Path>) -> Vec<FileStamp> {
        candidate_settings_files(workspace)
            .into_iter()
            .map(|path| {
                let stamp = metadata(&path)
                    .ok()
                    .and_then(|m| Some((m.modified().ok()?, m.len())));
                (path, stamp)
            })
            .collect()
    }

    fn refresh(&self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let cwd = self.active_cwd();
        let stamps = Self::read_stamps(cwd.as_deref());
        {
            let mut guard = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            if guard
                .as_ref()
                .is_some_and(|(prev_cwd, prev_stamps)| prev_cwd == &cwd && prev_stamps == &stamps)
            {
                return;
            }
            *guard = Some((cwd.clone(), stamps));
        }
        let user_settings = read_settings_value(&settings_path());
        let specs = collect_configured_specs(&user_settings, cwd.as_deref());
        install_configured_hooks(&registry, &specs, &self.launcher, cwd.as_deref());
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
    let (reg, _) = build_session_registry_with_reloader(gate, launcher, workspace);
    reg
}

fn build_session_registry_with_reloader(
    gate: Arc<dyn ModeGate>,
    launcher: Arc<dyn ProcessLauncher>,
    workspace: Option<&Path>,
) -> (Arc<HookRegistry>, Arc<ConfigHookReloader>) {
    let user_settings = read_settings_value(&settings_path());
    let all_specs = collect_configured_specs(&user_settings, workspace);
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
    let reg = Arc::new(HookRegistry::with_policy(policy));
    install_configured_hooks(&reg, &all_specs, &launcher, workspace);
    reg.register(Arc::new(SkillGrantHook::new(gate)));
    let reloader = ConfigHookReloader::new(&reg, launcher, workspace);
    let reloader_cb = Arc::clone(&reloader);
    reg.set_reloader(Arc::new(move |_| reloader_cb.refresh()));
    (reg, reloader)
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

/// The skill registry, conditional activator, grant store, and hook registrar
/// for one session.
pub(super) struct SkillRuntime {
    pub(super) registry: Arc<SkillRegistryImpl>,
    pub(super) conditional: Arc<dyn ConditionalSkillActivator>,
    pub(super) active_skill: Arc<Mutex<Option<String>>>,
    pub(super) grants: Option<Arc<SkillGrantStore>>,
    pub(super) hook_registry: Arc<HookRegistry>,
    pub(super) hook_reloader: Arc<ConfigHookReloader>,
    pub(super) registrar: Arc<SkillHookRegistrar>,
}

impl SkillRuntime {
    /// Discover skills, load grants, and build the session hook registry.
    pub(super) fn new(workspace: Option<&Path>, gate: Arc<dyn ModeGate>) -> Self {
        let (registry, conditional) = build_skill_registry_and_activator(workspace);
        let active_skill = Arc::new(Mutex::new(None));
        let grants = build_skill_grants();
        let launcher: Arc<dyn ProcessLauncher> = Arc::new(StdProcessLauncher::new());
        let (hook_registry, hook_reloader) =
            build_session_registry_with_reloader(gate, Arc::clone(&launcher), workspace);
        let trust_state = Arc::new(RwLock::new(TrustState::Untrusted));
        let registrar = Arc::new(SkillHookRegistrar::new(
            Arc::clone(&hook_registry),
            trust_state,
            launcher,
        ));
        Self {
            registry,
            conditional,
            active_skill,
            grants,
            hook_registry,
            hook_reloader,
            registrar,
        }
    }

    pub(super) fn discovery_skill(&self) -> Option<Arc<Mutex<Option<String>>>> {
        self.grants.as_ref().map(|_| Arc::clone(&self.active_skill))
    }

    pub(super) fn register_tool(
        &self,
        tools: &mut ToolRegistry,
        sandbox: Option<Arc<dyn SandboxSession>>,
    ) {
        register_skill_tool(
            tools,
            &self.registry,
            &self.registrar,
            &self.conditional,
            sandbox,
            self.grants.clone(),
            Arc::clone(&self.active_skill),
        );
    }

    /// The multi-agent deps this session's parent components yield. Gathered
    /// here because the child-inherited components live on this runtime: a
    /// child runs the parent's tools, so it needs the skill registry and
    /// activator (which skills exist, which gated ones are active) and the
    /// hook registry (the write-time gates). A child without them holds the
    /// Skill tool with nothing behind it and bypasses every project rule.
    #[expect(clippy::too_many_arguments, reason = "one call site, all parent parts")]
    pub(super) fn multi_agent_deps(
        &self,
        registry: Arc<dyn AgentRegistry>,
        store: Arc<dyn SessionLog>,
        provider: Arc<dyn ModelProvider>,
        tools: ToolRegistry,
        config: RunnerConfig,
        worktree_controller: Option<Arc<WorktreeController>>,
        workspace: Option<PathBuf>,
        bus: Option<Arc<AgentBus>>,
        descriptor_store: Option<Arc<dyn SessionDescriptorStore>>,
    ) -> crate::composition::multi_agent::MultiAgentDeps {
        crate::composition::multi_agent::MultiAgentDeps {
            registry,
            store,
            provider,
            tools,
            config,
            worktree_controller,
            workspace,
            bus,
            descriptor_store,
            skill_registry: Some(Arc::clone(&self.registry) as Arc<dyn SkillRegistry>),
            conditional: Some(Arc::clone(&self.conditional)),
            hooks: Some(Arc::clone(&self.hook_registry)),
        }
    }

    pub(super) fn attach_to_runner(&self, runner: Runner, workspace: Option<PathBuf>) -> Runner {
        if let Some(ws) = &workspace {
            *runner.cwd_handle().write().expect("cwd lock") = ws.clone();
        }
        self.hook_reloader.bind_runner_cwd(runner.cwd_handle());
        let mut runner = runner
            .with_skill_registry(Arc::clone(&self.registry) as Arc<dyn SkillRegistry>)
            .with_skill_grants(self.grants.clone())
            .with_active_skill(Arc::clone(&self.active_skill))
            .with_conditional(Arc::clone(&self.conditional))
            .with_hooks(Arc::clone(&self.hook_registry))
            .with_registrar(Arc::clone(&self.registrar));
        runner.set_skill_reloader(build_skill_reloader(
            &self.registry,
            &self.conditional,
            &self.registrar,
            workspace,
        ));
        runner
    }
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
                    if let Ok(rule) = rule
                        && let Err(error) = self.gate.add_rule(rule.with_scope(Scope::Session))
                    {
                        tracing::warn!(
                            "a trusted skill's allowed tool could not be granted: {error}"
                        );
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
mod tests;
