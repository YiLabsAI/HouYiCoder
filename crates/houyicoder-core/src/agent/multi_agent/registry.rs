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

use houyicoder_protocol::llm::EffortLevel;

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
        disallowed_tools: Vec::new(),
        model: None,
        effort: None,
        permission_mode: None,
        max_turns: None,
        skills: Vec::new(),
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        initial_prompt: None,
        memory: MemoryScope::Disabled,
        isolation: IsolationMode::None,
        omit_project_context: false,
        color: None,
        system_prompt: PromptSource::Owned(
            "You are a sub-agent. Complete the assigned task fully using your tools — not gold-plated, not half-done. When done, reply with a concise report: what you did and any key findings. The caller relays your report, so it only needs the essentials. Prefer editing existing files over creating new ones; do not create documentation files unless explicitly asked.".to_string()
        ),
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
        max_turns: None,
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
        max_turns: None,
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
        max_turns: None,
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
        max_turns: None,
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
mod tests {
    use super::{
        AgentError, AgentRegistry, BuiltInRegistry, ResolveCtx, built_in_all,
        built_in_general_purpose,
    };

    /// A registered built-in must resolve to its definition, unchanged.
    #[test]
    fn test_registry_resolves_builtin() {
        let reg = BuiltInRegistry::from_agents(vec![built_in_general_purpose()]);
        let def = reg
            .resolve("general-purpose", &ResolveCtx::default())
            .expect("registered type must resolve");
        assert_eq!(def.subagent_type, "general-purpose");
        assert!(!def.when_to_use.is_empty());
    }

    /// An unregistered type must surface the available set so the caller can
    /// report it, rather than panicking or returning a bare not-found.
    #[test]
    fn test_unregistered_lists_available() {
        let reg = BuiltInRegistry::from_agents(vec![built_in_general_purpose()]);
        let err = reg
            .resolve("nonexistent", &ResolveCtx::default())
            .expect_err("unregistered type must error");
        match err {
            AgentError::NotFound { available, .. } => {
                assert!(available.contains(&"general-purpose".to_string()));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// A deny hit must return a distinct error from not-registered. The two
    /// causes drive different caller behavior: an absent type is a usage
    /// mistake; a denied type is a policy decision that may need escalation.
    #[test]
    fn test_denied_returns_permission_denied() {
        let reg = BuiltInRegistry::from_agents(vec![built_in_general_purpose()]);
        let ctx = ResolveCtx::default().with_denied(["general-purpose".to_string()]);
        let err = reg
            .resolve("general-purpose", &ctx)
            .expect_err("denied type must error");
        assert!(matches!(err, AgentError::PermissionDenied { .. }));
    }

    /// A type that is denied but not registered must be NotFound, not
    /// PermissionDenied. Denying a non-existent name would leak that the
    /// name is on the deny list and report a policy decision for a type the
    /// caller cannot use anyway.
    #[test]
    fn test_deny_skips_unregistered() {
        let reg = BuiltInRegistry::from_agents(vec![built_in_general_purpose()]);
        let ctx = ResolveCtx::default().with_denied(["ghost".to_string()]);
        let err = reg
            .resolve("ghost", &ctx)
            .expect_err("unregistered type must error");
        assert!(matches!(err, AgentError::NotFound { .. }));
    }

    /// list returns every registered definition so the caller can build the
    /// catalog injection or a user-facing roster.
    #[test]
    fn test_list_returns_all_registered() {
        let reg = BuiltInRegistry::from_agents(vec![built_in_general_purpose()]);
        let all = reg.list();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].subagent_type, "general-purpose");
    }

    /// explore is read-only: the file-mutating tools and the sub-agent tool
    /// are denied, and it runs on the fast model tier (search is throughput,
    /// not deep reasoning).
    #[test]
    fn test_explore_disallows_write() {
        let def = super::built_in_explore();
        assert!(def.disallowed_tools.contains(&"write".to_string()));
        assert!(def.disallowed_tools.contains(&"edit".to_string()));
        assert!(def.disallowed_tools.contains(&"agent".to_string()));
        assert_eq!(def.model.as_deref(), Some("Flash"));
    }

    #[test]
    fn test_plan_model_is_max() {
        assert_eq!(super::built_in_plan().model.as_deref(), Some("Max"));
    }

    #[test]
    fn test_verify_model_is_max() {
        assert_eq!(super::built_in_verify().model.as_deref(), Some("Max"));
    }

    #[test]
    fn test_guide_uses_flash() {
        assert_eq!(super::built_in_code_guide().model.as_deref(), Some("Flash"));
    }

    /// built_in_all returns the five built-ins in a stable order so catalog
    /// injection is byte-stable across sessions.
    #[test]
    fn test_builtin_count_five() {
        let all = built_in_all();
        assert_eq!(all.len(), 5);
        let types: Vec<&str> = all.iter().map(|d| d.subagent_type.as_str()).collect();
        assert_eq!(
            types,
            ["general-purpose", "explore", "plan", "verify", "code-guide"]
        );
    }

    /// Read-only search agents fan out frequently and never act on project
    /// rules, so their user context drops the project memory; write-bearing
    /// agents keep it.
    #[test]
    fn test_explore_plan_omit_memory() {
        assert!(super::built_in_explore().omit_project_context);
        assert!(super::built_in_plan().omit_project_context);
        assert!(!super::built_in_general_purpose().omit_project_context);
        assert!(!super::built_in_verify().omit_project_context);
    }
}
