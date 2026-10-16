//! Dependency-injection assembly. The single site in the whole workspace that
//! constructs concrete engine implementations: the Runner, the model provider,
//! the tool registry and its tools, the sandbox session, the memory backend,
//! the session store, and the permission gate. Only the binary entry calls
//! into this module; nothing else constructs these. The build_runner logic
//! that lived in the TUI moves here so the TUI becomes a pure protocol
//! client.

#![allow(dead_code)] // composition root consumed by other crates; locally unused

pub(crate) mod catalog_resolver;
pub mod fleet_status_relay;
pub mod host_lifecycle;
pub mod notification_drain;
use catalog_resolver::SettingsCatalogResolver;
pub use catalog_resolver::{effort_to_persist, persist_model_pick};
mod built_in_tools;
mod hook_compose;
mod memory;
pub mod multi_agent;
mod reloader;
mod resume;
mod retention_notice;
mod session_descriptor;
mod skill;
pub use skill::SkillRegistryImpl;
mod startup_warnings;
mod worktree;

mod containment;
mod paths;
pub(crate) use containment::ContainmentAdapter;
use containment::build_session_fences;

pub use resume::{
    ResumeError, build_runner_for_fork, build_runner_for_resume_export,
    build_runner_for_resume_sid, latest_session_sid,
};
use std::sync::Arc;

use tokio::sync::Notify;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::session::SessionLog;
use houyicoder_api::trust::TrustState;
use houyicoder_config::{
    ConfigWarning, ModelSelection, load_model_section_from, resolve_model_selection_from,
    settings_path,
};
use houyicoder_context::ContextBackend;
use houyicoder_context::SessionDescriptorStore;
use houyicoder_context::SessionId;
use houyicoder_core::agent::auto_dream::{DEFAULT_DREAM_MAX_TURNS, DreamRunner};
use houyicoder_core::agent::extractor::{ExtractionLogs, MemoryExtractor};
use houyicoder_core::agent::model_window;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{
    CommandHook, ConversationSearchTool, DelegationTool, GitWorkspaceProbe, HookPolicy,
    HookRegistry, HookSource, HotPathReducer, LlmSummarizer, Runner, TodoWriteTool, ToolRegistry,
    parse_event,
};
use houyicoder_core::snapshot::{SnapshotStore, UndoStack};
use houyicoder_memory::{
    FileDescriptorStore, InMemoryBackend, InMemoryDescriptorStore, LocalFileBackend,
};
use houyicoder_permission::{DefaultModeGate, ModeGate, RuleStore};
use houyicoder_protocol::frontend::model::SpeedMode;
use houyicoder_session::SessionStore;

pub use houyicoder_provider::{ProviderFacts, ProviderResolution};

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::RwLock;

use crate::lifecycle::SessionLeaseStore;
use crate::server::EventSequencer;
use built_in_tools::BuiltInToolProvider;
use hook_compose::SkillRuntime;
use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_api::mcp::McpClient;
use houyicoder_api::mcp::McpToolProvider;
use houyicoder_api::tool::ToolProvider;
use houyicoder_core::agent::WorktreeController;
use houyicoder_core::agent::build_hook_fire;
use houyicoder_core::agent::multi_agent::bus_types::AgentBus;
use houyicoder_core::agent::multi_agent::registry::AgentRegistry;
pub use paths::{
    resolve_project_workspace, session_log_root, walk_to_workspace_root, workspace_cwd,
};

/// Give up on a capability, and say so, returning the absence as None.
/// Every degradation point routes through one function so the decision and the
/// disclosure stay inseparable — both args required so the failure alone does
/// not tell the user what stopped working.
fn degrade_with_notice<T, E: Display>(
    attempt: Result<T, E>,
    what_failed: &str,
    consequence: &str,
) -> Option<T> {
    match attempt {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!("{what_failed}: {e}; {consequence}");
            None
        }
    }
}
use std::collections::HashSet;
use std::env::var;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU32;

/// The runner plus the handles the host needs to render host-side state
/// without dispatching a tool call. Returned by build_runner and assemble;
/// the resume variants return ResumedRunner (this plus the restored model).
///
/// Deliberately not non-exhaustive, unlike the option structs: this is what
/// the root builds, so no caller constructs one, and the marker would only
/// stop a consumer from taking the whole value it was just handed.
pub struct AssembledRunner {
    pub runner: Runner,
    pub session: SessionId,
    pub gate: Arc<DefaultModeGate>,
    pub sandbox_session: Option<Arc<dyn SandboxSession>>,
    pub append_notify: Arc<Notify>,
    pub worktree_controller: Option<Arc<WorktreeController>>,
    /// The shared multi-agent bus; threaded to the pairing point so the
    /// fleet status relay can subscribe + emit agent status wire frames.
    pub bus: Option<Arc<AgentBus>>,
    /// What the provider was built from. Carried on the assembled runner rather
    /// than threaded separately: the resolution happens inside this root, and a
    /// caller that had to re-derive the facts would be a second source for the
    /// endpoint the status view reports.
    pub provider_facts: ProviderFacts,
}

/// An assembled runner plus the model restored from a resumed session's
/// descriptor. The fresh path (build_runner) does not return a model — the
/// caller resolves it from the config layer; the resume path must return
/// the model the session was using, which may differ from the current
/// config default.
#[non_exhaustive]
pub struct ResumedRunner {
    pub assembled: AssembledRunner,
    pub model: String,
}

/// Options for build_runner: None on a store field selects the in-memory
/// default; persistence is opt-in, named at every production entry.
#[non_exhaustive]
#[derive(Default)]
pub struct BuildRunnerOptions {
    pub project: Option<String>,
    pub rule_store: Option<Arc<dyn RuleStore>>,
    pub backend: Option<Box<dyn ContextBackend>>,
    pub descriptor_store: Option<Arc<dyn SessionDescriptorStore>>,
    /// None resolves one here; a caller reusing a provider across sessions
    /// passes its own.
    pub provider: Option<ProviderResolution>,
}

impl BuildRunnerOptions {
    /// Both stores on disk at the sid-keyed root when production persistence is opted in.
    pub fn disk(project: Option<String>, rule_store: Option<Arc<dyn RuleStore>>) -> Self {
        Self::disk_at(session_log_root(), project, rule_store)
    }

    /// The same preset at an explicit root, for a test isolating its session dir.
    pub fn disk_at(
        root: PathBuf,
        project: Option<String>,
        rule_store: Option<Arc<dyn RuleStore>>,
    ) -> Self {
        Self {
            project,
            rule_store,
            backend: Some(Box::new(LocalFileBackend::new(root.clone()))),
            descriptor_store: Some(Arc::new(FileDescriptorStore::new(root))),
            provider: None,
        }
    }
}

/// Build a descriptor store at the configured sessions root.
pub fn disk_descriptor_store() -> Arc<dyn SessionDescriptorStore> {
    Arc::new(FileDescriptorStore::new(session_log_root()))
}

/// Build a descriptor store at an explicit sessions root.
pub fn disk_descriptor_store_at(root: PathBuf) -> Arc<dyn SessionDescriptorStore> {
    Arc::new(FileDescriptorStore::new(root))
}

/// Build the Runner plus the handles the host needs to render host-side
/// state without dispatching a tool call. Returns the runner, the fresh
/// session id, and the shared permission gate (so the host can drive mode
/// changes and surface the rule set). The checklist renders from the wire
/// stream, so no shared todo handle is returned. Provider + model
/// resolution lives in the config layer; when no key is set a stub
/// provider keeps the loop driving with a canned reply, never silently
/// going offline.
pub fn build_runner(options: BuildRunnerOptions) -> AssembledRunner {
    let append_notify = Arc::new(Notify::new());
    let backend = options
        .backend
        .unwrap_or_else(|| Box::new(InMemoryBackend::new()));
    let session = SessionId::new();
    let model_selection = resolve_model_selection_from(&settings_path());
    let model = model_selection.model.clone();
    let descriptor_store = options
        .descriptor_store
        .unwrap_or_else(|| Arc::new(InMemoryDescriptorStore::new()));
    let project = options.project;
    let initial_descriptor =
        session_descriptor::build_initial_descriptor(&model, project.as_deref());
    let store = SessionStore::new(backend)
        .with_append_notify(append_notify.clone())
        .with_first_durable(session_descriptor::materialize_hook(
            Arc::clone(&descriptor_store),
            initial_descriptor,
        ));
    let store = Arc::new(store);
    let resolved = options
        .provider
        .unwrap_or_else(|| resolve_provider(project.as_deref()));
    assemble(
        store,
        session,
        model_selection,
        project,
        options.rule_store,
        append_notify,
        resolved,
        descriptor_store,
    )
}

/// Assemble a runner over a pre-built store, session, and model.
///
/// Kept as separate inputs: they come from different places (the store and the
/// append signal are built for this session, the project and rule store arrive
/// from the caller, the provider is already resolved), and the store is
/// consumed by the runner while the session and append signal are handed back
/// out, so no single group holds them. A struct would be named for this
/// function rather than for a concept.
#[expect(
    clippy::too_many_arguments,
    reason = "the inputs come from different places and are not one concept"
)]
pub(crate) fn assemble(
    store: Arc<SessionStore>,
    session: SessionId,
    model_selection: ModelSelection,
    project: Option<String>,
    rule_store: Option<Arc<dyn RuleStore>>,
    append_notify: Arc<Notify>,
    resolved: ProviderResolution,
    descriptor_store: Arc<dyn SessionDescriptorStore>,
) -> AssembledRunner {
    let model = model_selection.model;
    let ProviderResolution {
        provider,
        facts: provider_facts,
        warnings: provider_cfg_warnings,
    } = resolved;
    let provider_for_extractor = Arc::clone(&provider);
    let agent_registry = multi_agent::built_in_registry();
    let recall_meter = Arc::new(AtomicU32::new(0));
    let mut tools = build_base_tools(&store, &recall_meter, &agent_registry);
    let fences = build_session_fences(project, rule_store.clone());
    let gate_dyn: Arc<dyn ModeGate> = fences.gate.clone();
    let worktree_controller = worktree::wire_worktree_controller(
        fences.workspace.as_deref(),
        fences.sandbox_session.as_ref(),
        &store,
        session,
        &mut tools,
        &gate_dyn,
    );
    let skills = SkillRuntime::new(fences.workspace.as_deref(), gate_dyn.clone());
    let undo_handles = register_builtin_tools(&mut tools, &fences, &skills, gate_dyn);
    let (config, runner_settings_warnings) =
        build_runner_config(model.clone(), fences.workspace.as_deref());
    skills.register_tool(&mut tools, fences.sandbox_session.clone());
    let denied_agents = resolve_denied_agents(rule_store.as_deref());
    let bus = Arc::new(AgentBus::new());
    let spawn_handle = multi_agent::build_runtime(multi_agent::MultiAgentDeps {
        registry: agent_registry.clone(),
        store: store.clone(),
        provider: Arc::clone(&provider),
        tools: tools.clone(),
        config: config.clone(),
        worktree_controller: worktree_controller.clone(),
        workspace: fences.workspace.clone(),
        bus: Some(Arc::clone(&bus)),
        descriptor_store: Some(Arc::clone(&descriptor_store)),
    });
    let (section, effort_warnings) = load_model_section_from(&settings_path());
    let store_log_root = store.session_log_root();
    let runner = Runner::new(store, provider, tools, config)
        .with_recall_meter(recall_meter)
        .with_breaker(fences.breaker)
        .with_summarizer(Box::new(LlmSummarizer::new(
            Arc::clone(&provider_for_extractor),
            model.clone(),
        )))
        .with_catalog_resolver(Arc::new(SettingsCatalogResolver::from_section(
            section.clone(),
        )))
        .with_model_choice(model_selection.choice)
        .with_speed_mode(section.speed_mode.unwrap_or(SpeedMode::Standard))
        .with_denied_agents(Arc::clone(&denied_agents))
        .with_spawn_handle(spawn_handle)
        .with_sandbox_session(fences.sandbox_session.clone())
        .with_reducer(Arc::new(HotPathReducer));
    runner.seed_usage_from_log(session);
    let mut runner = skills.attach_to_runner(runner, fences.workspace.clone());
    runner.set_workspace_probe(Arc::new(GitWorkspaceProbe::new(runner.cwd_handle())));
    multi_agent::wire_agent_directory(&runner, agent_registry.as_ref(), &denied_agents);
    if let Some(controller) = &worktree_controller {
        controller.set_hook_fire(build_hook_fire(&runner));
    }
    let (runner, toggle_warnings) = install_workspace_memory(
        runner,
        fences.workspace,
        session,
        provider_for_extractor,
        model,
        undo_handles,
        worktree_controller.as_ref(),
    );
    let mut startup = startup_warnings::collect_startup_warnings(
        &fences.network_warnings,
        &toggle_warnings,
        &effort_warnings,
        &provider_cfg_warnings,
    );
    finish_startup_warnings(
        &mut startup,
        &runner_settings_warnings,
        fences.sandbox_session.as_ref(),
        store_log_root,
        session,
    );
    AssembledRunner {
        runner: runner.with_startup_warnings(startup),
        session,
        gate: fences.gate,
        sandbox_session: fences.sandbox_session,
        append_notify,
        worktree_controller,
        bus: Some(bus),
        provider_facts,
    }
}

/// The tools every session starts with: the todo writer, the search over this
/// session's own log, and delegation to the built-in agent types. The recall
/// meter stays with the caller because the runner reports through the same one.
fn build_base_tools(
    store: &Arc<SessionStore>,
    recall_meter: &Arc<AtomicU32>,
    agent_registry: &Arc<dyn AgentRegistry>,
) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    let session_log: Arc<dyn SessionLog> = store.clone();
    tools.register(Arc::new(TodoWriteTool::new()));
    tools.register(Arc::new(ConversationSearchTool::new(
        session_log,
        Arc::clone(recall_meter),
    )));
    tools.register(Arc::new(DelegationTool::new(Arc::clone(agent_registry))));
    tools
}

/// Build the RunnerConfig and collect any settings warnings from runner_settings.
fn build_runner_config(
    model: String,
    workspace: Option<&Path>,
) -> (RunnerConfig, Vec<ConfigWarning>) {
    let max_output_tokens = model_window::resolve_max_output_tokens(&model);
    let (runner_settings, warnings) = houyicoder_config::load_runner_settings(workspace);
    let config = RunnerConfig {
        model,
        instructions: String::new(),
        max_turns: runner_settings.max_turns,
        max_output_tokens,
        ..RunnerConfig::default()
    };
    (config, warnings)
}

/// Extract denied sub-agent types from the rule store.
fn resolve_denied_agents(rule_store: Option<&dyn RuleStore>) -> Arc<HashSet<String>> {
    Arc::new(match rule_store {
        Some(rules) => houyicoder_permission::denied_agent_types(&rules.load()),
        None => HashSet::new(),
    })
}

/// Append runner-settings warnings, unfenced sandbox notices, and retention backlog notices.
fn finish_startup_warnings(
    startup: &mut Vec<String>,
    runner_settings_warnings: &[ConfigWarning],
    sandbox_session: Option<&Arc<dyn SandboxSession>>,
    store_log_root: Option<PathBuf>,
    session: SessionId,
) {
    startup.extend(
        runner_settings_warnings
            .iter()
            .map(|w| format!("{}: {}", w.field, w.reason)),
    );
    if var(houyicoder_config::ENV_HOUYICODER_TEST_SUPPRESS_FENCE_NOTICE).map_or(true, |v| v != "1")
        && let Some(session) = sandbox_session
        && let Some(notice) = session.fence_status().unfenced_notice()
    {
        startup.push(notice);
    }
    retention_notice::push_startup_notices(store_log_root, session, startup);
}

/// Wire workspace-scoped memory, undo handles, and the real cwd handle onto the runner.
fn install_workspace_memory(
    runner: Runner,
    workspace: Option<PathBuf>,
    session: SessionId,
    provider_for_extractor: Arc<dyn ModelProvider>,
    model_for_extractor: String,
    undo_handles: Option<(Arc<Mutex<UndoStack>>, Arc<SnapshotStore>)>,
    worktree_controller: Option<&Arc<WorktreeController>>,
) -> (Runner, Vec<ConfigWarning>) {
    match workspace {
        Some(ws) => {
            let memory_provider: Arc<dyn MemoryProvider> =
                Arc::new(memory::memory_provider_for(&ws, None));
            let session_log_root = runner.store().session_log_root();
            let (runtime, warnings) = memory::build_memory_runtime(
                runner.store(),
                session,
                memory_provider,
                provider_for_extractor,
                ws.clone(),
                model_for_extractor,
                session_log_root,
            );
            let mut r = runner.with_cwd(ws).install_memory(runtime);
            if let Some((stack, store)) = undo_handles {
                r.set_undo(stack, store);
            }
            if let Some(controller) = worktree_controller {
                controller.set_cwd_handle(r.cwd_handle());
            }
            (r, warnings)
        }
        None => (runner, Vec::new()),
    }
}

/// Register the sandbox-backed built-in tools, and hand back the undo handles
/// they expose so the workspace branch can attach them to the runner.
fn register_builtin_tools(
    tools: &mut ToolRegistry,
    fences: &containment::SessionFences,
    skills: &SkillRuntime,
    gate_dyn: Arc<dyn ModeGate>,
) -> Option<(Arc<Mutex<UndoStack>>, Arc<SnapshotStore>)> {
    let builtin = BuiltInToolProvider::new(fences.sandbox_session.clone(), gate_dyn)
        .with_activator(Some(Arc::clone(&skills.conditional)))
        .with_active_skill(skills.discovery_skill());
    let undo_handles = builtin.undo_handles();
    register_provider_tools(tools, builtin);
    undo_handles
}

/// Register the built-in tool provider and any configured MCP tool servers.
fn register_provider_tools(tools: &mut ToolRegistry, builtin: BuiltInToolProvider) {
    let mut providers: Vec<Box<dyn ToolProvider>> = vec![Box::new(builtin)];
    let launcher = StdProcessLauncher::new();
    for cfg in houyicoder_config::resolve_mcp_servers() {
        match McpClient::spawn(&cfg.program, &cfg.args, &launcher) {
            Ok((client, entries)) => {
                providers.push(Box::new(McpToolProvider::new(client, entries)));
            }
            Err(e) => {
                tracing::warn!(
                    "external tool server {:?} failed to start: {e}; \
                     skipping (built-in tools still load)",
                    cfg.program
                );
            }
        }
    }
    for provider in &providers {
        for tool in provider.tools() {
            tools.register(tool);
        }
    }
}

/// The provider a project runs on: the workspace this process is held to,
/// then the adapter crate's resolution of the settings that apply to it.
///
/// Choosing the workspace is this root's decision, so it is made here and
/// handed over; a resolver that also walked the tree would own two questions.
pub fn resolve_provider(project: Option<&str>) -> ProviderResolution {
    let workspace = resolve_project_workspace(project.map(str::to_string));
    houyicoder_provider::resolve(workspace.as_deref())
}

/// Runner and event-stream state retained across client connections. The
/// sequencer preserves reliable history, sequence allocation, and the durable
/// log cursor while each attachment supplies a new carrier.
struct LiveRunnerHandle {
    runner: Arc<Runner>,
    event_sequencer: EventSequencer,
    gate: Arc<dyn ModeGate>,
    append_notify: Arc<Notify>,
}

/// The session-indexed host a reattaching connection re-hydrates from. Holds
/// the live runners + cursors (so the Arc<Runner> survives a disconnect) and
/// the SessionLeaseStore (the single source of truth for the parked
/// PendingTurn + the lease holder + the lifecycle state). serve_session
/// reads the handle for this session, rebuilds a Server, and drives serve;
/// when the serve ends on disconnect the host retains everything.
///
/// The gate is per-runner (composition), so it lives in the handle, not on
/// the host — a multi-session host holds one gate per live runner.
pub struct SessionHost {
    runners: Mutex<HashMap<SessionId, LiveRunnerHandle>>,
    store: SessionLeaseStore,
    /// The facts the process's provider was built from. Process-scoped
    /// rather than per-session because the provider is resolved once for the
    /// life of a process; serve_session reads it so a reattached connection
    /// reports the same facts the live one does.
    provider_facts: Option<ProviderFacts>,
}

impl SessionHost {
    /// In-memory host with no persisted backing. The single-process
    /// fast path: a new process cannot reload sessions (cross-process
    /// reconnect is the UDS cut).
    pub fn new(store: SessionLeaseStore) -> Self {
        Self {
            runners: Mutex::new(HashMap::new()),
            store,
            provider_facts: None,
        }
    }

    /// Attach what the process's provider was built from, so a reattaching
    /// connection's status reports the real destination.
    pub fn with_provider_facts(mut self, facts: ProviderFacts) -> Self {
        self.provider_facts = Some(facts);
        self
    }

    /// The facts a reattaching connection should report, or None when the
    /// host was built without them.
    pub(crate) fn provider_facts(&self) -> Option<&ProviderFacts> {
        self.provider_facts.as_ref()
    }

    /// Register a runner and its session-scoped event sequencer.
    pub fn insert(
        &self,
        session: SessionId,
        runner: Arc<Runner>,
        event_sequencer: EventSequencer,
        gate: Arc<dyn ModeGate>,
        append_notify: Arc<Notify>,
    ) {
        self.runners.lock().expect("host lock").insert(
            session,
            LiveRunnerHandle {
                runner,
                event_sequencer,
                gate,
                append_notify,
            },
        );
    }

    /// Clone the runner, sequencer, gate, and append notification needed by a
    /// reattaching connection. None when the session is not registered.
    pub(crate) fn clone_handle(&self, session: SessionId) -> Option<RunnerHandleClone> {
        self.runners
            .lock()
            .expect("host lock")
            .get(&session)
            .map(|h| RunnerHandleClone {
                runner: h.runner.clone(),
                event_sequencer: h.event_sequencer.clone(),
                gate: h.gate.clone(),
                append_notify: h.append_notify.clone(),
            })
    }

    /// Single source of truth for the parked PendingTurn, lease holder, and
    /// state. pub(crate) so serve_session runs the lease guard.
    pub(crate) fn store(&self) -> &SessionLeaseStore {
        &self.store
    }
}

/// A cloned snapshot of a session's live handle. pub(crate) so the
/// serve_session entry point re-hydrates a Server from it without the
/// host exposing its internal LiveRunnerHandle.
pub(crate) struct RunnerHandleClone {
    pub(crate) runner: Arc<Runner>,
    pub(crate) event_sequencer: EventSequencer,
    pub(crate) gate: Arc<dyn ModeGate>,
    pub(crate) append_notify: Arc<Notify>,
}

#[cfg(test)]
mod tests;
