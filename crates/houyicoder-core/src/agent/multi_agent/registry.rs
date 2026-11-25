//! Agent definition + registry contract.
//!
//! A sub-agent is a typed, declaratively-described unit of delegation: the
//! parent runner hands it a prompt and a narrowed tool set, it runs to a
//! terminal status, and its result flows back. The registry is the single
//! source of truth for what agent types exist and how to materialize one.
//!
//! Two failure causes are kept distinct because they call for different
//! caller behavior: an absent type is a usage mistake (show the available
//! set), while a denied type is a policy decision (may need escalation).
//! Collapsing them hides the real cause from the caller.

use std::collections::HashSet;
use std::iter::once;

use houyicoder_protocol::llm::EffortLevel;

/// The mutating memory tools that must never be accessible to any child runner.
pub const MUTATING_MEMORY_TOOLS: &[&str] = &[
    "memory_write",
    "memory_delete",
    "memory_archive",
    "memory_promote",
    "memory_demote",
];

/// Where a sub-agent runs relative to the parent's working tree.
///
/// None shares the parent's tree. The per-child worktree fence is the
/// default for write-bearing delegation so a stray edit cannot land in the
/// parent's tree before the result is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolationMode {
    #[default]
    None,
    Worktree,
}

/// Whether and where a sub-agent's experience sediment lands. Sedimentation
/// itself lands later; the field is part of the definition shape now so the
/// registry does not change when it switches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryScope {
    #[default]
    Disabled,
    User,
    Project,
    Local,
}

/// Source of the sub-agent's system instructions.
///
/// Owned carries a self-contained prompt (the built-ins and user-defined
/// markdown bodies). InheritParent is the fork path: the child reuses the
/// parent's rendered prompt bytes so the two share a prompt-cache prefix.
/// Fork is gated behind a spike before it lands; the variant is here so the
/// type is exhaustive from day one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSource {
    Owned(String),
    InheritParent,
}

/// A fully-described sub-agent type, materialized from a built-in, a plugin,
/// or a markdown frontmatter file. Fields the runtime does not yet act on
/// are still parsed so a definition file never silently drops a setting;
/// they switch on when their owning feature lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    pub subagent_type: String,
    pub when_to_use: String,
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
    pub model: Option<String>,
    pub effort: Option<EffortLevel>,
    /// Permission mode token as written in the definition (e.g. "manual",
    /// "auto"). Resolved to a real permission mode at the composition root,
    /// where the permission crate is available; core stays free of that
    /// layer so the agent loop never couples to the gate implementation.
    pub permission_mode: Option<String>,
    pub max_turns: Option<u32>,
    pub skills: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub hooks: Vec<String>,
    pub initial_prompt: Option<String>,
    pub memory: MemoryScope,
    pub isolation: IsolationMode,
    /// Drop the project memory file (AGENTS.md equivalent) from the child's
    /// user context. Read-heavy agents fan out frequently and never act on
    /// project rules, so the flag opts them out of paying the project-memory
    /// tokens on every spawn.
    pub omit_project_context: bool,
    pub color: Option<String>,
    pub system_prompt: PromptSource,
    /// How the sub-agent executes: in the parent's process or as an external
    /// ACP process. Defaults to in-process so every existing definition stays
    /// compatible without a field change.
    pub backend: AgentBackend,
}

/// How a sub-agent runs. In the parent process, or as an external ACP child.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentBackend {
    /// Share the parent's runtime and heap, the only path before external ACP
    /// support landed. All built-in and user-defined agents default here.
    #[default]
    InProcess,
    /// Spawn an external ACP-compatible process and drive it as a child. The
    /// spec carries the program, arguments, allowed environment variables, and
    /// handshake timeout the host uses to launch and fence it.
    ExternalAcp(ExternalAcpSpec),
}

/// Configuration for an external ACP sub-agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalAcpSpec {
    /// The executable to launch, resolved on PATH or as an absolute path.
    pub program: String,
    /// Argv after the program name.
    pub args: Vec<String>,
    /// Host environment variables to pass through, in addition to the base
    /// runtime set. No variable outside this list and the base set crosses the
    /// process boundary, so a host secret never leaks to a child.
    pub passthrough_env: Vec<String>,
    /// Variables this definition sets on every child it launches, whatever
    /// the host environment holds. They cross the boundary in addition to the
    /// passthrough list, so a user can configure the external program from the
    /// definition instead of exporting state for it.
    pub env: Vec<(String, String)>,
    /// Milliseconds to wait for the external process to finish its initialize
    /// and session/new handshake before declaring it unresponsive.
    pub handshake_timeout_ms: u64,
}

impl Default for ExternalAcpSpec {
    fn default() -> Self {
        Self {
            program: String::new(),
            args: Vec::new(),
            passthrough_env: Vec::new(),
            env: Vec::new(),
            handshake_timeout_ms: 15_000,
        }
    }
}

/// The environment variables a child process receives. The base runtime set
/// the process needs to function, the ones the spec passes through from the
/// host environment, and the ones it sets itself, which win over both. No
/// other host variable crosses the boundary.
pub fn child_environment(
    spec: &ExternalAcpSpec,
    host_env: &[(String, String)],
) -> Vec<(String, String)> {
    const BASE_VARS: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "SYSTEMROOT",
        "COMSPEC",
    ];
    let allowed: HashSet<&str> = BASE_VARS
        .iter()
        .copied()
        .chain(spec.passthrough_env.iter().map(String::as_str))
        .collect();
    let mut env: Vec<(String, String)> = host_env
        .iter()
        .filter(|(key, _)| allowed.contains(key.as_str()))
        .filter(|(_, value)| !value.is_empty())
        .cloned()
        .collect();
    for (key, value) in &spec.env {
        env.retain(|(existing, _)| existing != key);
        env.push((key.clone(), value.clone()));
    }
    env
}

/// Why a resolve call did not yield a definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// The requested type is not registered. The available list carries what
    /// is registered, so the caller can surface a useful message rather than
    /// a bare not-found.
    NotFound {
        requested: String,
        available: Vec<String>,
    },
    /// The type exists but a deny rule blocks it. Distinct from NotFound
    /// because a denial is a policy decision, not a usage mistake.
    PermissionDenied { denied_type: String },
}

/// Per-resolve context the registry consults for policy. Carries the agent
/// types denied by permission rules at the call site (the Agent(x) deny
/// form), consulted after a type is confirmed registered.
#[derive(Debug, Clone, Default)]
pub struct ResolveCtx {
    denied: HashSet<String>,
}

impl ResolveCtx {
    /// Mark the named agent types as denied for this resolve.
    pub fn with_denied<I>(mut self, types: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        self.denied = types.into_iter().collect();
        self
    }
}

/// The registry the runtime and the agent tool consult. Implementations may
/// layer built-in, plugin, user, project, and managed sources with the
/// precedence defined in the loader; this trait only fixes the query shape.
pub trait AgentRegistry: Send + Sync {
    fn resolve(&self, subagent_type: &str, ctx: &ResolveCtx)
    -> Result<AgentDefinition, AgentError>;

    fn list(&self) -> Vec<AgentDefinition>;
}

/// One line of the agent directory injected into the parent system prompt:
/// the type, its when-to-use, and (when the definition pins a tool set) the
/// tools. A definition that inherits the parent's tools (tools = None) shows
/// no tool suffix - the when-to-use already describes the agent.
pub fn format_agent_line(def: &AgentDefinition) -> String {
    let tools = match &def.tools {
        Some(ts) if !ts.is_empty() => format!(" (Tools: {})", ts.join(", ")),
        _ => String::new(),
    };
    format!("- {}: {}{}", def.subagent_type, def.when_to_use, tools)
}

/// The registered types the model may delegate to, minus any a deny rule
/// blocks, sorted by type. Membership and order come from here alone, so the
/// prompt section and the /agents reply cannot disagree about which types
/// exist or what order they are read in.
pub fn delegatable_agents(
    registry: &dyn AgentRegistry,
    denied: &HashSet<String>,
) -> Vec<AgentDefinition> {
    let mut defs: Vec<AgentDefinition> = registry
        .list()
        .into_iter()
        .filter(|d| !denied.contains(&d.subagent_type))
        .collect();
    defs.sort_by(|a, b| a.subagent_type.cmp(&b.subagent_type));
    defs
}

/// The agent directory section for the parent system prompt: one line per
/// definition. None when there is nothing to announce. Rendering only — the
/// bytes stay stable within a session because the definitions are installed
/// once and the order is already fixed by the caller.
pub fn agent_directory_section(defs: &[AgentDefinition]) -> Option<String> {
    if defs.is_empty() {
        return None;
    }
    let lines: Vec<String> = defs.iter().map(format_agent_line).collect();
    Some(format!("## Available agents\n\n{}", lines.join("\n")))
}

/// A registry backed by a fixed vector of definitions. Enough for the
/// built-ins and for tests; the precedence-layered loader builds on this.
pub struct BuiltInRegistry {
    agents: Vec<AgentDefinition>,
}

impl BuiltInRegistry {
    pub fn from_agents(agents: Vec<AgentDefinition>) -> Self {
        Self { agents }
    }
}

impl AgentRegistry for BuiltInRegistry {
    fn resolve(
        &self,
        subagent_type: &str,
        ctx: &ResolveCtx,
    ) -> Result<AgentDefinition, AgentError> {
        // Registration before deny: a type that is not even registered is
        // NotFound regardless of any deny rule -- denying a non-existent name
        // would both leak that the name is deny-listed and report a policy
        // decision for something the caller cannot use anyway.
        let found = self
            .agents
            .iter()
            .find(|d| d.subagent_type == subagent_type)
            .cloned();
        let Some(def) = found else {
            return Err(AgentError::NotFound {
                requested: subagent_type.to_string(),
                available: self
                    .agents
                    .iter()
                    .map(|d| d.subagent_type.clone())
                    .collect(),
            });
        };
        if ctx.denied.contains(subagent_type) {
            return Err(AgentError::PermissionDenied {
                denied_type: subagent_type.to_string(),
            });
        }
        Ok(def)
    }

    fn list(&self) -> Vec<AgentDefinition> {
        self.agents.clone()
    }
}

/// The default workhorse: a fully-capable, no-specialization sub-agent for
/// multi-step research, search, and execution when no specialized type fits.
/// Inherits the parent's model and full tool set; carries a self-contained
/// prompt so it does not depend on the parent's rendered instructions.
pub fn built_in_general_purpose() -> AgentDefinition {
    AgentDefinition {
        subagent_type: "general-purpose".to_string(),
        when_to_use: "General-purpose agent for researching complex questions, searching for code, and executing multi-step tasks. Delegate here when you are unsure where a keyword or file lives, or when no specialized agent fits.".to_string(),
        tools: None,
        disallowed_tools: once("agent".to_string())
            .chain(MUTATING_MEMORY_TOOLS.iter().map(|s| s.to_string()))
            .collect(),
        model: None,
        effort: None,
        permission_mode: None,
        // A general-purpose child does the whole assignment, so it gets the
        // budget a short task needs. The ceiling exists so a runaway search
        // stops visibly rather than spending a session's tokens on nothing.
        max_turns: Some(200),
        skills: Vec::new(),
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        initial_prompt: None,
        memory: MemoryScope::Project,
        isolation: IsolationMode::None,
        omit_project_context: false,
        color: None,
        system_prompt: PromptSource::Owned(
            "You are a sub-agent. Complete the assigned task fully using your tools — not gold-plated, not half-done. When done, reply with a concise report: what you did and any key findings. The caller relays your report, so it only needs the essentials. Prefer editing existing files over creating new ones; do not create documentation files unless explicitly asked.".to_string()
        ),
        backend: AgentBackend::default(),
    }
}

/// Read-only codebase search. Disallows every file-mutating tool and the
/// sub-agent tool (no nesting); runs on the fast model tier because search
/// is high-throughput, low-reasoning work.
pub fn built_in_explore() -> AgentDefinition {
    AgentDefinition {
        subagent_type: "explore".to_string(),
        when_to_use: "Fast read-only agent for exploring a codebase: finding files by pattern, searching code for keywords, answering how parts of the system work. Caller specifies thoroughness: quick / medium / very thorough.".to_string(),
        tools: None,
        disallowed_tools: vec!["write".into(), "edit".into(), "multiedit".into(), "agent".into()],
        model: Some("Flash".into()),
        effort: None,
        permission_mode: None,
        // A read-only child answers one question. Its budget stops a
        // search that keeps finding new leads from consuming the whole
        // session, while leaving room for a real multi-file sweep.
        max_turns: Some(100),
        skills: Vec::new(),
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        initial_prompt: None,
        memory: MemoryScope::Disabled,
        isolation: IsolationMode::None,
        omit_project_context: true,
        color: None,
        system_prompt: PromptSource::Owned(
            "You are a read-only codebase search agent. Find files and code by pattern or keyword, read and analyze, report findings concisely. You cannot create, modify, or delete files. Be fast: prefer parallel tool calls, return findings as soon as you have them. Match search breadth to the thoroughness the caller named.".to_string()
        ),
        backend: AgentBackend::default(),
    }
}

/// Read-only software architect. Explores the codebase, finds existing
/// patterns, designs an implementation plan. Runs on the strongest model
/// tier because architectural reasoning is the whole point.
pub fn built_in_plan() -> AgentDefinition {
    AgentDefinition {
        subagent_type: "plan".to_string(),
        when_to_use: "Read-only architect agent: explores the codebase and designs a step-by-step implementation plan for a task, identifying critical files and trade-offs. Use before writing code when the approach is not obvious.".to_string(),
        tools: None,
        disallowed_tools: vec!["write".into(), "edit".into(), "multiedit".into(), "agent".into()],
        model: Some("Max".into()),
        effort: None,
        permission_mode: None,
        // A read-only child answers one question. Its budget stops a
        // search that keeps finding new leads from consuming the whole
        // session, while leaving room for a real multi-file sweep.
        max_turns: Some(100),
        skills: Vec::new(),
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        initial_prompt: None,
        memory: MemoryScope::Disabled,
        isolation: IsolationMode::None,
        omit_project_context: true,
        color: None,
        system_prompt: PromptSource::Owned(
            "You are a read-only software architect. Explore the codebase, find existing patterns and conventions, and design an implementation plan for the given requirements. You cannot modify files. End your response with a 'Critical Files for Implementation' section listing 3-5 files most relevant to executing the plan, each with a one-line reason.".to_string()
        ),
        backend: AgentBackend::default(),
    }
}

/// Adversarial verification agent. Tries to break the implementation, not
/// confirm it. Strongest model tier: finding real bugs is harder than
/// writing the first 80%. Output ends with a machine-parsed VERDICT line.
pub fn built_in_verify() -> AgentDefinition {
    AgentDefinition {
        subagent_type: "verify".to_string(),
        when_to_use: "Adversarial verification agent that tries to break an implementation rather than confirm it. Runs real commands, checks outputs, probes edge cases. Returns a machine-parsed VERDICT. Use before claiming work done, especially for security- or correctness-critical changes.".to_string(),
        tools: None,
        disallowed_tools: vec!["write".into(), "edit".into(), "multiedit".into(), "agent".into()],
        model: Some("Max".into()),
        effort: None,
        permission_mode: None,
        // A read-only child answers one question. Its budget stops a
        // search that keeps finding new leads from consuming the whole
        // session, while leaving room for a real multi-file sweep.
        max_turns: Some(100),
        skills: Vec::new(),
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        initial_prompt: None,
        memory: MemoryScope::Disabled,
        isolation: IsolationMode::None,
        omit_project_context: false,
        color: Some("red".into()),
        system_prompt: PromptSource::Owned(
            "You are an adversarial verification agent. Your job is to break the implementation, not confirm it. Two failure modes to avoid: (1) verification avoidance -- reading code, narrating what you would test, writing PASS without running; (2) being seduced by the first 80% -- a polished surface or passing suite hides the broken 20%. Run real commands, check outputs against expectations, probe edge cases the implementer did not. You may write ephemeral scripts to a temp directory but must not modify the project. End with a line: VERDICT: PASS or VERDICT: FAIL or VERDICT: PARTIAL.".to_string()
        ),
        backend: AgentBackend::default(),
    }
}

/// Houyi guide agent. Helps the user understand and use the tool itself --
/// configuration, hooks, skills, slash commands, settings, model selection.
/// Fast model tier: doc lookup is high-frequency, low-reasoning.
pub fn built_in_code_guide() -> AgentDefinition {
    AgentDefinition {
        subagent_type: "code-guide".to_string(),
        when_to_use: "Guide agent for the tool itself: configuration, hooks, skills, slash commands, settings, model selection. Fetches the docs map for authoritative answers.".to_string(),
        tools: Some(vec!["read".into(), "grep".into(), "glob".into(), "WebFetch".into(), "web_search".into()]),
        disallowed_tools: Vec::new(),
        model: Some("Flash".into()),
        effort: None,
        permission_mode: None,
        // A read-only child answers one question. Its budget stops a
        // search that keeps finding new leads from consuming the whole
        // session, while leaving room for a real multi-file sweep.
        max_turns: Some(100),
        skills: Vec::new(),
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        initial_prompt: None,
        memory: MemoryScope::Disabled,
        isolation: IsolationMode::None,
        omit_project_context: false,
        color: None,
        system_prompt: PromptSource::Owned(
            "You are the houyi guide agent. Help the user understand and use the tool: configuration, hooks, skills, slash commands, settings, model selection. Fetch the docs map for authoritative answers, keep answers actionable, cite the doc section.".to_string()
        ),
        backend: AgentBackend::default(),
    }
}

/// All built-in agents in registration order. Order is stable so catalog
/// injection (the directory the model sees) is byte-stable across sessions.
pub fn built_in_all() -> Vec<AgentDefinition> {
    vec![
        built_in_general_purpose(),
        built_in_explore(),
        built_in_plan(),
        built_in_verify(),
        built_in_code_guide(),
    ]
}

#[cfg(test)]
mod tests;
