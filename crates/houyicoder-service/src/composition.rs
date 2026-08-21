//! Dependency-injection assembly. The single site in the whole workspace that
//! constructs concrete engine implementations: the Runner, the model provider,
//! the tool registry and its tools, the sandbox session, the memory backend,
//! the session store, and the permission gate. Only the binary entry calls
//! into this module; nothing else constructs these. The build_runner logic
//! that lived in the TUI moves here so the TUI becomes a pure protocol
//! client.

#![allow(dead_code)] // composition root consumed by other crates; locally unused

mod effort_resolver;
pub use effort_resolver::{effort_to_persist, persist_model_pick};
mod hooks;
mod memory;
mod resume;
mod session_meta;
mod startup_warnings;
mod worktree;

mod containment;
mod paths;
pub(crate) use containment::{ContainmentAdapter, rehydrate_directories};

pub use resume::{
    ResumeError, build_runner_for_fork, build_runner_for_resume_export,
    build_runner_for_resume_sid, latest_session_sid, log_last_active_secs,
};
use std::sync::Arc;

use tokio::sync::Notify;

use houyicoder_api::launcher::ProcessLauncher;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::session::SessionLog;
use houyicoder_context::SessionId;
use houyicoder_context::SessionMetaStore;
use houyicoder_context::backend::ContextBackend;
use houyicoder_core::agent::auto_dream::{DEFAULT_DREAM_MAX_TURNS, DreamRunner};
use houyicoder_core::agent::extractor::MemoryExtractor;
use houyicoder_core::agent::model_window;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{
    AskUserQuestionTool, BashTool, CommandHook, ConversationSearchTool, EditTool,
    GitWorkspaceProbe, GlobTool, GrepTool, HookRegistry, HookSource, HotPathReducer, LlmSummarizer,
    MultiEditTool, ReadTool, Runner, TodoWriteTool, ToolRegistry, WebFetchTool, WriteTool,
    parse_event,
};
use houyicoder_memory::{FileMetaStore, InMemoryBackend, InMemoryMetaStore, LocalFileBackend};
use houyicoder_permission::{DefaultModeGate, GuardedTool, ModeGate, RuleStore};
use houyicoder_provider::{FakeProvider, OpenAiCompatibleProvider};
use houyicoder_resilience::resource_breaker::{ResourceBreaker, ResourceBreakerConfig};
use houyicoder_sandbox::PlatformSession;
use houyicoder_session::SessionStore;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

/// Give up on a capability, and say so, returning the absence as None.
///
/// Every point where the composition root can lose a capability has the same
/// three parts: an attempt that may fail, a consequence the user cannot infer
/// from the symptom, and a decision to carry on in a reduced form. Routing them
/// through one function keeps the decision and the disclosure inseparable, so a
/// new degradation cannot be introduced without naming what it costs. Both
/// arguments are required for that reason: the failure alone does not tell the
/// user what stopped working, which is the difference between a diagnosable
/// startup and an agent that just appears not to work.
pub use paths::{
    resolve_project_workspace, session_log_root, walk_to_workspace_root, workspace_cwd,
};

fn degrade_with_notice<T, E: std::fmt::Display>(
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

/// The runner plus the handles the host needs to render host-side state
/// without dispatching a tool call. Returned by build_runner and assemble;
/// the resume variants return ResumedRunner (this plus the restored model).
#[non_exhaustive]
pub struct AssembledRunner {
    pub runner: Runner,
    pub session: SessionId,
    pub gate: Arc<DefaultModeGate>,
    pub sandbox_session: Option<Arc<dyn SandboxSession>>,
    pub append_notify: Arc<Notify>,
    pub worktree_controller: Option<Arc<houyicoder_core::agent::WorktreeController>>,
}

/// An assembled runner plus the model restored from a resumed session's
/// sidecar. The fresh path (build_runner) does not return a model — the
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
    pub meta_store: Option<Arc<dyn SessionMetaStore>>,
}

impl BuildRunnerOptions {
    /// Production persistence opt-in: both stores on disk at the sid-keyed
    /// sessions root (still redirectable via the sessions-dir env).
    pub fn disk(project: Option<String>, rule_store: Option<Arc<dyn RuleStore>>) -> Self {
        Self::disk_at(session_log_root(), project, rule_store)
    }

    /// The same preset at an explicit root, for a caller that owns one (a
    /// test isolating its session dir) - the exact store wiring the binary
    /// entries get, without rebuilding it by hand.
    pub fn disk_at(
        root: std::path::PathBuf,
        project: Option<String>,
        rule_store: Option<Arc<dyn RuleStore>>,
    ) -> Self {
        Self {
            project,
            rule_store,
            backend: Some(Box::new(LocalFileBackend::new(root.clone()))),
            meta_store: Some(Arc::new(FileMetaStore::new(root))),
        }
    }
}

/// A disk meta store at the sid-keyed sessions root, for production entries
/// that read sidecars without assembling a runner (the session picker); the
/// composition root constructs stores so the binary never names the storage
/// crate directly.
pub fn disk_meta_store() -> Arc<dyn SessionMetaStore> {
    Arc::new(FileMetaStore::new(session_log_root()))
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
    // A caller mounting a specific backend/meta store (a resume path, or a
    // test that deliberately wants disk) passes it in.
    let backend = options
        .backend
        .unwrap_or_else(|| Box::new(InMemoryBackend::new()));
    let store = Arc::new(SessionStore::new(backend).with_append_notify(append_notify.clone()));
    let session = SessionId::new();
    let model = houyicoder_config::resolve_model();
    // Write the initial session.json sidecar at creation so /status can
    // show name/cwd/model/provenance + a later resume can restore them.
    let meta_store = options
        .meta_store
        .unwrap_or_else(|| Arc::new(InMemoryMetaStore::new()));
    let project = options.project;
    session_meta::write_initial_session_meta(&meta_store, session, &model, project.as_deref());
    assemble(
        store,
        session,
        model,
        project,
        options.rule_store,
        append_notify,
    )
}

#[expect(clippy::too_many_lines, reason = "composition root")]
/// Assemble a runner over a pre-built store + session + model. Shared by the
/// fresh-session path above and the resume path (the CLI mounts a
/// LocalFileBackend at an existing session log and seeds the trajectory
/// before assembling). Public so the CLI resume entry reuses the
/// provider/tools/gate/sandbox wiring without re-implementing it.
pub fn assemble(
    store: Arc<SessionStore>,
    session: SessionId,
    model: String,
    project: Option<String>,
    rule_store: Option<Arc<dyn RuleStore>>,
    append_notify: Arc<Notify>,
) -> AssembledRunner {
    let model_for_extractor = model.clone();
    let workspace = resolve_project_workspace(project.clone());
    let provider: Arc<dyn ModelProvider> = provider_or_stub(workspace.as_deref());
    // Clone before the provider is moved into the runner so the memory
    // extractor (wired in the workspace branch) shares the same provider
    // handle — shares the same prompt-cache prefix as the runner.
    let provider_for_extractor = Arc::clone(&provider);
    let mut tools = ToolRegistry::new();
    // The checklist tool holds its own session state (no sandbox needed), so
    // register it unconditionally — it is available even when the seatbelt is
    // unavailable. It is not destructive and needs no approval gate, so it
    // skips the GuardedTool wrapper that the filesystem tools require. The
    // host renders the checklist from the wire stream, so no state handle is
    // returned to the caller.
    let todo_tool = TodoWriteTool::new();
    tools.register(Arc::new(todo_tool));
    // The shared permission gate. Every tool is wrapped in a GuardedTool that
    // routes its calls through mode_gate.decide, so the mode (plan / default /
    // acceptEdits / auto / bypass) and the user-configurable whitelist rule
    // set decide allow / ask / deny — the runner and the Tool trait stay
    // untouched. Bypass skips approval; the sandbox still fences it. The same
    // Arc is returned to the caller so the host and the runner share one gate.
    // Wire the durable rule store into the gate: rules the user adds via
    // /permissions persist across restart, and with_store hydrates them into
    // the in-memory rule set on construction. Without this call the store is
    // dead code (add_rule's persist branch never fires), so /permissions rule
    // additions are lost on restart — only the builtins re-seed. None means
    // no persistence (test isolation: the wire-flow tests do not need a store
    // and must not touch the real home/project files).
    let mut gate_builder = DefaultModeGate::new();
    if let Some(ref rule_store) = rule_store {
        gate_builder = gate_builder.with_store(rule_store.clone());
    }
    // The network posture, read once per session and handed to the sandbox
    // profile below, the one consumer of it. One
    // origin on purpose — when the gate was told separately it could disagree
    // with the fence, so a user who opened the fence still had every egress
    // command refused. The gate no longer reads network posture; the fence
    // is the single authority over containment, and the gate asks for every
    // egress command regardless.
    let (network, network_warnings) =
        crate::sandbox_policy::network_policy_from(&houyicoder_config::load_sandbox_network());
    // Guarded mode (default): fence the user's project dir directly — the
    // agent edits the real tree (changes immediate) and the seatbelt denies
    // writes outside it + the network. Isolated (worktree) mode is opt-in via
    // the /sandbox command. Fall back to a tempdir sandbox when no cwd or the
    // seatbelt is unavailable.
    // One aggregate resource breaker shared across the whole run: every
    // spawned command the tools issue aggregates against it, so a runaway
    // (orphan-CPU) command trips Open and the next spawn is refused for the
    // cool-down. Attached to the shared session so every tool holding the
    // session Arc aggregates against one breaker — the composition root owns
    // the policy, the sandbox applies it (no trait change).
    let breaker = Arc::new(ResourceBreaker::new(ResourceBreakerConfig::default()));
    // Resolve the project root deterministically — NEVER trust the inherited
    // process cwd blindly. An installed binary launched from a shell sitting
    // in the home dir would otherwise make the seatbelt workspace the home
    // dir, so the agent's bash lists home instead of the repo (it cannot see
    // the code it is supposed to develop). Resolution order: the project env
    // override, then walk up from the cwd to the nearest workspace manifest,
    // then fall back to an isolated tempdir + a stderr warning (never the
    // home dir as workspace).
    let workspace = resolve_project_workspace(project);
    // The two session-wide fences every constructed session must carry: the
    // aggregate resource breaker and the network posture. Bound once here so the
    // three construction paths below cannot drift apart — a path that forgot the
    // network posture would silently fall back to the contained default and make
    // an opened fence look broken.
    let fence = |s: PlatformSession| {
        s.with_breaker(breaker.clone())
            .with_network(network.clone())
    };
    // Last resort when the workspace cannot be fenced. A failure here leaves the
    // run with no session, and every tool that needs one is then withheld, which
    // is the right posture and an opaque one: the agent has no way to touch the
    // filesystem or run a command, and nothing on screen says why.
    let tempdir_session = || {
        degrade_with_notice(
            PlatformSession::new(),
            "sandbox unavailable: no isolated tempdir session could be created",
            "the agent starts with no filesystem or command tools.",
        )
        .map(&fence)
    };
    let sandbox = match &workspace {
        // A workspace that cannot be fenced (a non-existent --project path, or a
        // canonicalize failure) degrades to the tempdir rather than to no fence.
        Some(ws) => degrade_with_notice(
            PlatformSession::new_in_cwd(ws),
            &format!("sandbox could not pin to workspace {ws:?}"),
            "falling back to an isolated tempdir -- the agent's bash will NOT see your repo.",
        )
        .map(&fence)
        .or_else(tempdir_session),
        None => {
            tracing::warn!(
                "no project root found (no Cargo.toml walking up from cwd, and HOUYICODER_PROJECT unset); sandboxing to an isolated tempdir -- the agent's bash will NOT see your repo. run from the repo root or pass --project <path>."
            );
            tempdir_session()
        }
    };
    let sandbox_session: Option<Arc<dyn SandboxSession>> =
        sandbox.map(|s| Arc::new(s) as Arc<dyn SandboxSession>);
    // Rehydrate persistent directory authorizations into the kernel fence.
    // Directories the user persisted (via /permissions AddDir or an approval
    // card) live in the rule store's envelope; the fence is in-memory and
    // starts empty, so without this loop a persistent directory auth is silent
    // on restart — the store has it, but the kernel fence does not, and the
    // tool still refuses. This bridges the two layers (store → fence). Errors
    // are ignored: a directory deleted since it was persisted should not brick
    // startup; the stale entry just does not re-attach.
    if let (Some(session), Some(store)) = (&sandbox_session, &rule_store) {
        rehydrate_directories(session.as_ref(), store.as_ref());
    }
    // Hand the gate a fence handle so the path-bounds validator can ask for
    // out-of-workspace grep/glob paths instead of letting confine_path refuse.
    if let Some(ref session) = sandbox_session {
        gate_builder = gate_builder.with_containment(Arc::new(ContainmentAdapter(session.clone())));
    }
    let gate = Arc::new(gate_builder);
    let gate_dyn: Arc<dyn ModeGate> = gate.clone();
    // Worktree controller: built + the enter/exit tools registered when both
    // a workspace + a sandbox session resolved. The controller's cwd handle
    // starts as a dummy; set_cwd_handle below swaps in the runner's real cwd.
    let worktree_controller = worktree::wire_worktree_controller(
        workspace.as_deref(),
        sandbox_session.as_ref(),
        &store,
        session,
        &mut tools,
        &gate_dyn,
    );
    // Assemble tools from providers. The composition root gathers providers
    // (built-in here; an external crate adds its own ToolProvider) and
    // registers each tool, so adding a tool set is adding a provider, not
    // editing the registry call list. TodoWriteTool is registered above
    // (it carries its own state and needs no sandbox); the rest come from
    // providers.
    let builtin = BuiltInToolProvider::new(sandbox_session.clone(), gate_dyn.clone());
    let undo_handles = builtin.undo_handles();
    let mut providers: Vec<Box<dyn houyicoder_api::tool::ToolProvider>> = vec![Box::new(builtin)];
    // External tool servers (block-on-init): spawn each subprocess via the
    // launcher, run the initialize plus tools/list handshake before the agent
    // loop starts, and contribute the discovered tools behind the Tool trait.
    // A spawn failure is fail-open: the engine still runs with the built-in
    // tools; the error is surfaced on stderr so a misconfigured server does
    // not brick the run. The tool list is fixed for the session (a server
    // that adds tools mid-session does not surface until restart).
    let launcher = houyicoder_api::launcher::StdProcessLauncher::new();
    for cfg in houyicoder_config::resolve_mcp_servers() {
        match houyicoder_api::mcp::McpClient::spawn(&cfg.program, &cfg.args, &launcher) {
            Ok((client, entries)) => {
                providers.push(Box::new(houyicoder_api::mcp::McpToolProvider::new(
                    client, entries,
                )));
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
    // An empty instructions lets the runner's ContextBuilder assemble the
    // system prompt from sections (identity + project context + tool docs +
    // env) each turn; a non-empty value would override that assembled prompt.
    // The assembled path is the default; the override is reserved for a future
    // CLI --system-prompt flag.
    let max_output_tokens = model_window::resolve_max_output_tokens(&model);
    let config = RunnerConfig {
        model,
        instructions: String::new(),
        max_turns: 200,
        max_output_tokens,
        ..RunnerConfig::default()
    };
    // Recall meter: shared between the conversation recall tool (bumps it on a
    // match in the folded span) and the compaction path (snapshots + resets
    // to compute the recall rate). Constructed here so the tool + the runner
    // share one Arc; passed to the tool at registration + to the runner via
    // with_recall_meter before the runner is shared.
    let recall_meter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    // The conversation recall tool is not sandbox-backed (it replays the raw
    // session log the runner holds), so it is registered directly like the
    // checklist tool, not via the sandbox-built-in provider. Shares the store
    // + the recall meter so its bumps land on the counter the compaction path
    // reads.
    let conversation_search = ConversationSearchTool::new(store.clone(), recall_meter.clone());
    tools.register(Arc::new(conversation_search));
    // LlmSummarizer shares the main provider + model so compress produces
    // real summaries; the self-overflow guard + heuristic fallback are in
    // lifecycle.rs. Cloned before the runner takes the provider.
    let summarizer = Box::new(LlmSummarizer::new(
        Arc::clone(&provider_for_extractor),
        model_for_extractor.clone(),
    ));
    let (effort_resolver, effort_warnings) =
        effort_resolver::SettingsEffortResolver::load_with_warnings();
    let mut runner = Runner::with_shared_store(store, provider, tools, config)
        .with_recall_meter(recall_meter)
        .with_breaker(breaker)
        .with_summarizer(summarizer)
        .with_effort_resolver(std::sync::Arc::new(effort_resolver));
    // Wire a workspace probe for the re-derivable compaction backbone's
    // derivation watermark. Shares the runner's cwd handle so a worktree
    // switch propagates to the next probe. Set after the builder chain (the
    // cwd handle exists on the built runner); mirrors set_live_sink's pre-Arc
    // mutation. Best-effort: the probe returns None when the cwd is not a git
    // repo, so a probe failure never fails a compaction.
    runner.set_workspace_probe(std::sync::Arc::new(GitWorkspaceProbe::new(
        runner.cwd_handle(),
    )));
    // Wire the hot-path tool-output reducer so the Isolate stage strips ansi
    // + truncates a large bash result before serving it (the raw stays in the
    // CAS for on-demand retrieval).
    runner = runner.with_reducer(std::sync::Arc::new(HotPathReducer));
    // External command hooks: resolve the specs from the same env-config path
    // as external tool servers, build a CommandHook per spec through the same
    // process-launcher chokepoint (the clippy spawn ban routes every spawn
    // there), and attach the registry so the runner's fire points drive them.
    // A spec with an unknown event name is skipped with a stderr warning; an
    // empty or unset config yields no registry, so the fire points run with
    // no external verdict source and the engine still runs.
    let hook_launcher: Arc<dyn ProcessLauncher> =
        Arc::new(houyicoder_api::launcher::StdProcessLauncher::new());
    let hooks = hooks::build_hook_registry(&houyicoder_config::resolve_hooks(), hook_launcher);
    let runner = match hooks {
        Some(registry) => runner.with_hooks(registry),
        None => runner,
    };
    let (runner, toggle_warnings) = match workspace {
        Some(ws) => {
            let memory_provider: Arc<dyn MemoryProvider> =
                Arc::new(memory::memory_provider_for(&ws));
            let cwd = ws.clone();
            let r = runner
                .with_cwd(ws)
                .with_memory(Arc::clone(&memory_provider));
            // Background memory (extractor + dream) at query-loop end, off
            // the hot path. None on the forked runner, no self-trigger.
            let (mut r, warnings) = memory::wire_background_memory(
                r,
                provider_for_extractor,
                memory_provider,
                cwd,
                model_for_extractor,
            );
            if let Some((stack, store)) = undo_handles {
                r.set_undo(stack, store);
            }
            // Swap in the runner's real cwd handle so worktree enter/exit write
            // the ContextBuilder cwd the next system-prompt build reads.
            if let Some(controller) = &worktree_controller {
                controller.set_cwd_handle(r.cwd_handle());
            }
            (r, warnings)
        }
        None => (runner, Vec::new()),
    };
    // Queue startup warnings (bad settings fields + network-policy typos) for
    // the host to drain + surface as initial transcript system lines at pair
    // time. A bad value must not silently become a no-op.
    let mut startup = startup_warnings::collect_startup_warnings(
        &network_warnings,
        &toggle_warnings,
        &effort_warnings,
    );
    // The sandbox fence status is a one-time construction event the user
    // must know: an unfenced workspace is a security-relevant gap. Check
    // once here rather than per-operation (the per-operation audit lines
    // stay in the tracing sink as diagnostics). Suppressed in test/PTY
    // environments via HOUYICODER_QUIET_FENCE so the notice does not occupy a
    // transcript line that PTY assertions must account for.
    if std::env::var("HOUYICODER_QUIET_FENCE").map_or(true, |v| v != "1")
        && let Some(session) = &sandbox_session
        && let Some(notice) = session.fence_status().unfenced_notice()
    {
        startup.push(notice);
    }
    let runner = runner.with_startup_warnings(startup);
    AssembledRunner {
        runner,
        session,
        gate,
        sandbox_session,
        append_notify,
        worktree_controller,
    }
}

/// Resolve the provider via the config layer: a real OpenAiCompatibleProvider
/// when a key env var is set, else a FakeProvider so the loop still drives
/// with a canned reply. Named for what it returns, not how (env resolution
/// lives in the config layer).
fn provider_or_stub(workspace: Option<&std::path::Path>) -> Arc<dyn ModelProvider> {
    // Test affordance: a scripted response sequence lets PTY UI tests drive
    // tool calls (glob / edit / todo_write) through the real binary so the
    // interaction layer — permission cards, tool-result rendering, transcript
    // fold — is exercised end-to-end. The env holds a JSON array of per-call
    // output-item lists; the first response typically carries a ToolCall, the
    // rest plain text so the run ends cleanly (a stateless stub re-emits the
    // same ToolCall every call and loops to max_turns). Falls through to the
    // normal stub path on any parse failure. Honest test knob, not a feature
    // — the stub providers exist for dev/test.
    if let Ok(raw) = std::env::var("HOUYICODER_STUB_SCRIPT")
        && let Ok(per_call) =
            serde_json::from_str::<Vec<Vec<houyicoder_protocol::llm::OutputItem>>>(&raw)
        && !per_call.is_empty()
    {
        return Arc::new(houyicoder_provider::FakeProvider::from_outputs(per_call));
    }
    match houyicoder_config::settings_merge::load_provider_merged(workspace) {
        Ok(cfg) => Arc::new(OpenAiCompatibleProvider::new(cfg.base_url, cfg.api_key)),
        Err(_) => {
            let reply = format!(
                "stub mode: no api key set, model {} not called. \
                 set DASHSCOPE_API_KEY in .env for real replies.",
                houyicoder_config::DEFAULT_MODEL
            );
            Arc::new(FakeProvider::text(&reply))
        }
    }
}

/// The built-in tool set: AskUserQuestion always, plus the sandboxed
/// filesystem + search + network tools (wrapped through the permission
/// gate) when a sandbox is present. One ToolProvider so the composition
/// root assembles its tools the same way it assembles any external
/// provider's. Adding a built-in tool is editing this provider; adding an
/// external tool set is adding a different provider, not touching this one.
struct BuiltInToolProvider {
    session: Option<Arc<dyn SandboxSession>>,
    gate: Arc<dyn ModeGate>,
    undo_stack: Option<Arc<std::sync::Mutex<houyicoder_core::snapshot::UndoStack>>>,
    snapshot_store: Option<Arc<houyicoder_core::snapshot::SnapshotStore>>,
}

impl BuiltInToolProvider {
    fn new(session: Option<Arc<dyn SandboxSession>>, gate: Arc<dyn ModeGate>) -> Self {
        let (undo_stack, snapshot_store) = match &session {
            Some(s) => {
                let store = degrade_with_notice(
                    houyicoder_core::snapshot::SnapshotStore::new(s.workspace_root()),
                    "snapshot store init failed",
                    "undo unavailable; destructive bash commands will require explicit approval",
                )
                .map(Arc::new);
                // Re-link the undo stack to surviving on-disk snapshots so a
                // resumed session can /undo a destructive op from the prior
                // process: the in-memory stack is lost on restart, but the
                // snap-N dirs persist. An empty stack (store init failed, or
                // no surviving snapshots) leaves /undo as no-op, same as before.
                let stack = Arc::new(std::sync::Mutex::new(match &store {
                    Some(st) => {
                        houyicoder_core::snapshot::UndoStack::from_entries(st.relink_undo_entries())
                    }
                    None => houyicoder_core::snapshot::UndoStack::new(),
                }));
                (Some(stack), store)
            }
            None => (None, None),
        };
        Self {
            session,
            gate,
            undo_stack,
            snapshot_store,
        }
    }

    /// The shared undo stack (cloned into the BashTool; also set on the Runner
    /// for /undo). None when no session or the snapshot store failed.
    fn undo_handles(
        &self,
    ) -> Option<(
        Arc<std::sync::Mutex<houyicoder_core::snapshot::UndoStack>>,
        Arc<houyicoder_core::snapshot::SnapshotStore>,
    )> {
        self.undo_stack
            .as_ref()
            .zip(self.snapshot_store.as_ref())
            .map(|(s, st)| (s.clone(), st.clone()))
    }
}

impl houyicoder_api::tool::ToolProvider for BuiltInToolProvider {
    fn name(&self) -> &str {
        "builtin"
    }

    fn tools(&self) -> Vec<Arc<dyn houyicoder_api::tool::Tool>> {
        let mut v: Vec<Arc<dyn houyicoder_api::tool::Tool>> =
            vec![Arc::new(AskUserQuestionTool::new())];
        if let Some(session) = &self.session {
            let bash = match (&self.undo_stack, &self.snapshot_store) {
                (Some(stack), Some(store)) => {
                    BashTool::with_undo(session.clone(), stack.clone(), store.clone())
                }
                _ => BashTool::new(session.clone()),
            };
            v.push(Arc::new(GuardedTool::new(
                Arc::new(bash),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(ReadTool::new(session.clone())),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(WriteTool::new(session.clone())),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(EditTool::new(session.clone())),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(MultiEditTool::new(session.clone())),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(GlobTool::new(session.clone())),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(GrepTool::new(session.clone())),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(WebFetchTool::new()),
                self.gate.clone(),
            )));
        }
        v
    }
}

/// The live-runner + cursor state a session retains across a client
/// disconnect. Held by SessionHost keyed by session id. The runner is
/// Arc-shared so a new serve on a reattaching connection resumes against
/// the same in-memory Runner (its SessionLog holds the parked tool calls);
/// next_seq is the same Arc the live delta sink fetch_adds from, so the
/// monotonic seq stream a reconnecting client resumes from survives; and
/// pushed_count is the trajectory cursor (MVP: same-client-reattach
/// semantics — a fresh-client full-replay cursor lands with the UDS cut).
struct LiveRunnerHandle {
    runner: Arc<Runner>,
    next_seq: Arc<AtomicU64>,
    pushed_count: usize,
    gate: Arc<dyn ModeGate>,
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
    store: crate::lifecycle::SessionLeaseStore,
}

impl SessionHost {
    /// In-memory host with no persisted backing. The single-process
    /// fast path: a new process cannot reload sessions (cross-process
    /// reconnect is the UDS cut).
    pub fn new(store: crate::lifecycle::SessionLeaseStore) -> Self {
        Self {
            runners: Mutex::new(HashMap::new()),
            store,
        }
    }

    /// Register a live runner + its shared seq counter + gate for a session.
    /// The composition root calls this once when it spawns a runner; the
    /// pushed_count starts at zero (no events pushed to any client yet).
    pub fn insert(
        &self,
        session: SessionId,
        runner: Arc<Runner>,
        next_seq: Arc<AtomicU64>,
        gate: Arc<dyn ModeGate>,
    ) {
        self.runners.lock().expect("host lock").insert(
            session,
            LiveRunnerHandle {
                runner,
                next_seq,
                pushed_count: 0,
                gate,
            },
        );
    }

    /// Clone the live handle for a session (the runner Arc, the shared seq
    /// counter, the pushed-event cursor, the gate) so a reattaching serve can
    /// rebuild a Server without the host surrendering its own clone. None when
    /// no live runner is registered for the session (cross-process reconnect
    /// without a checkpoint is the deferred Gap B).
    pub(crate) fn clone_handle(&self, session: SessionId) -> Option<RunnerHandleClone> {
        self.runners
            .lock()
            .expect("host lock")
            .get(&session)
            .map(|h| RunnerHandleClone {
                runner: h.runner.clone(),
                next_seq: h.next_seq.clone(),
                pushed_count: h.pushed_count,
                gate: h.gate.clone(),
            })
    }

    /// The lifecycle store (the single source of truth for the parked
    /// PendingTurn + the lease holder + the state). pub(crate) so the
    /// serve_session entry point can run the lease guard + read the pending
    /// turn without the host exposing its internal map.
    pub(crate) fn store(&self) -> &crate::lifecycle::SessionLeaseStore {
        &self.store
    }

    /// Write the pushed-event cursor back into the session's live handle. The
    /// disconnect paths in serve flush this so a reattaching connection does
    /// not re-send the trajectory log the prior client already saw.
    pub(crate) fn set_pushed_count(&self, session: SessionId, count: usize) {
        if let Some(h) = self.runners.lock().expect("host lock").get_mut(&session) {
            h.pushed_count = count;
        }
    }
}

/// A cloned snapshot of a session's live handle. pub(crate) so the
/// serve_session entry point re-hydrates a Server from it without the host
/// exposing its internal LiveRunnerHandle.
pub(crate) struct RunnerHandleClone {
    pub(crate) runner: Arc<Runner>,
    pub(crate) next_seq: Arc<AtomicU64>,
    pub(crate) pushed_count: usize,
    pub(crate) gate: Arc<dyn ModeGate>,
}

#[cfg(test)]
#[path = "composition_tests.rs"]
mod tests;
