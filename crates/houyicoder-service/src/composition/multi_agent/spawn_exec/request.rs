//! Building the child's spawn request and mapping spawn failures.
//!
//! One builder for both run modes so the foreground and background paths cannot
//! drift apart on what a child carries.

use super::super::last_assistant_text;
use super::*;
use houyicoder_api::session::SessionLog;
use houyicoder_core::agent::multi_agent::PartialTranscript;

use houyicoder_core::agent::ToolRegistry;
use houyicoder_permission::GuardedTool;

use crate::mcp::LIST_RESOURCES_TOOL;
use crate::mcp::ListMcpResourcesTool;
use crate::mcp::READ_RESOURCE_TOOL;
use crate::mcp::ReadMcpResourceTool;

pub(super) fn map_registry_err(error: AgentError) -> SpawnFailure {
    match error {
        AgentError::NotFound { .. } => SpawnFailure::UnknownAgent,
        AgentError::PermissionDenied { .. } => SpawnFailure::CapabilityDenied,
    }
}

/// How the child runs, as the caller decided before any side effect: what
/// started it, how deep it sits, where it runs, the token that ends it, and
/// whether the turn waits on it.
///
/// These five travel together because a caller that gets one of them from
/// somewhere other than the request is a caller that has lost track of which
/// delegation it is starting.
pub(super) struct ChildRunSpec {
    pub(super) trigger: TriggerSource,
    pub(super) depth: u32,
    pub(super) isolation: IsolationMode,
    pub(super) cancel: CancellationToken,
    pub(super) mode: ChildRunMode,
}

/// Reads a child's last assistant text from the session log, so an ending cut
/// short reports what the child had produced rather than the stop reason.
struct ChildTranscript {
    store: Arc<dyn SessionLog>,
}

impl PartialTranscript for ChildTranscript {
    fn last_text(&self, session: SessionId) -> Option<String> {
        last_assistant_text(&self.store.trajectory_tail(session))
    }
}

/// Build the child's spawn request from the resolved definition: the child
/// prompt, the restricted tool set, and the parent's shared components. One
/// builder for both run modes so the foreground and background paths cannot
/// drift apart on what a child carries.
///
/// A resume id the caller cannot parse is refused here, before the spawn has
/// any side effect: silently starting a fresh child would report success for a
/// continuation that never happened.
/// The tool scope a child runs with: the definition's disallowed names, the
/// servers it mounts, and whether it may only read. An absent server list
/// inherits the parent's; an explicit empty list mounts none.
/// The tool scope a child runs with: the definition's disallowed names, the
/// servers it mounts, and whether it may only read. An absent server list
/// inherits the parent's; an explicit empty list mounts none. A child that
/// withholds the writing tools is treated as a reader, so external tools
/// that can write stay out of its reach.
fn child_tool_scope(def: &AgentDefinition) -> (Vec<String>, Option<Vec<String>>, bool) {
    let read_only = def.disallowed_tools.iter().any(|entry| {
        let leaf = entry.rsplit("__").next().unwrap_or(entry);
        ["write", "edit", "multiedit"]
            .iter()
            .any(|writing| leaf.eq_ignore_ascii_case(writing))
    });
    (
        def.disallowed_tools.clone(),
        def.mcp_servers.clone(),
        read_only,
    )
}

/// The registry a child runs on: the base set the caller assembled (the
/// parent's tools plus the child's memory tools) narrowed by the scope, and
/// the resource tools scoped to the servers the definition mounts. The
/// parent's unscoped resource tools never travel: narrow copies them from
/// the static set, so they are always disallowed here and replaced by the
/// scoped variants the child may have.
fn child_tools(
    parent: &MultiAgentRuntime,
    def: &AgentDefinition,
    base: ToolRegistry,
    mut disallowed: Vec<String>,
) -> ToolRegistry {
    let (_, servers, read_only) = child_tool_scope(def);
    disallowed.push(LIST_RESOURCES_TOOL.to_string());
    disallowed.push(READ_RESOURCE_TOOL.to_string());
    let mut tools = base.narrow(&disallowed, servers.as_deref(), read_only);
    let Some(manager) = &parent.mcp else {
        return tools;
    };
    // An explicit empty list mounts nothing, so it reaches no resource
    // either; inheriting or a named list gets the bounded tools.
    if servers.as_ref().is_some_and(Vec::is_empty) {
        return tools;
    }
    let Some(gate) = &parent.mode_gate else {
        return tools;
    };
    let log = Arc::clone(&parent.store);
    let list = ListMcpResourcesTool::new(Arc::clone(manager), Some(Arc::clone(&log)))
        .with_allowed_servers(servers.clone());
    let read =
        ReadMcpResourceTool::new(Arc::clone(manager), Some(log)).with_allowed_servers(servers);
    tools.register(Arc::new(GuardedTool::new(Arc::new(list), Arc::clone(gate))));
    tools.register(Arc::new(GuardedTool::new(Arc::new(read), Arc::clone(gate))));
    tools
}

pub(super) fn build_spawn_request(
    this: &MultiAgentRuntime,
    parent_sid: SessionId,
    args: &SpawnArgs,
    spec: ChildRunSpec,
    def: &AgentDefinition,
) -> Result<SpawnRequest, SpawnFailure> {
    let base_prompt = match &def.system_prompt {
        PromptSource::Owned(p) => p.clone(),
        PromptSource::InheritParent => this.config.instructions.clone(),
    };
    let memory_allowed = def.memory != AgentMemoryScope::Disabled
        && this
            .inherited
            .memory_gates
            .as_ref()
            .is_none_or(MemoryGates::auto_memory_enabled);
    let child_memory = if memory_allowed {
        this.inherited.memory_provider.as_ref().cloned()
    } else {
        None
    };
    let memory_section = child_memory.as_deref().and_then(render_child_memory);
    let child_config = RunnerConfig {
        instructions: build_child_prompt(
            &base_prompt,
            &this.cwd,
            &this.config.model,
            def.omit_project_context,
            memory_section.as_deref(),
        ),
        max_turns: def.max_turns.unwrap_or(this.config.max_turns),
        ..this.config.clone()
    };
    let mut disallowed = def.disallowed_tools.clone();
    for tool in MUTATING_MEMORY_TOOLS {
        if !disallowed.iter().any(|d| d == *tool) {
            disallowed.push(tool.to_string());
        }
    }
    if !memory_allowed {
        disallowed.push("memory_read".to_string());
        disallowed.push("memory_search".to_string());
    }
    let mut child_registry = this.tools.clone();
    if let Some(mem) = child_memory {
        child_registry.register(Arc::new(MemoryReadTool::new(Arc::clone(&mem))));
        child_registry.register(Arc::new(
            MemorySearchTool::new(mem).with_reranker(this.inherited.memory_reranker.clone()),
        ));
    }
    let resume = match args.resume.as_deref() {
        Some(raw) => Some(SessionId::from_display_string(raw).ok_or_else(|| {
            SpawnFailure::InvalidResumeTarget {
                reason: "the id is not a session id".to_string(),
            }
        })?),
        None => None,
    };
    Ok(SpawnRequest {
        parent_sid,
        parent_store: this.store.clone(),
        provider: this.provider.clone(),
tools: child_tools(this, def, child_registry, disallowed),
        config: child_config,
        subagent_type: args.subagent_type.clone(),
        prompt: args.prompt.clone(),
        prompt_summary: args.prompt_summary.clone(),
        trigger: spec.trigger,
        depth: spec.depth,
        isolation: spec.isolation,
        worktree_controller: this.worktree_controller.clone(),
        run_mode: spec.mode,
        child_cancel: spec.cancel,
        bus: this.bus.clone(),
        descriptor_store: this.descriptor_store.clone(),
        skill_registry: this.skill_registry.clone(),
        conditional: this.conditional.clone(),
        hooks: this.hooks.clone(),
        inherited: this.inherited.clone(),
        partial_transcript: Some(Arc::new(ChildTranscript {
            store: Arc::clone(&this.store),
        })),
        resume,
        timeout_secs: args.timeout_secs,
    })
}

pub(super) fn map_spawn_err(e: SpawnError) -> SpawnFailure {
    match e {
        SpawnError::BudgetExceeded => SpawnFailure::BudgetExceeded,
        SpawnError::CapabilityDenied => SpawnFailure::CapabilityDenied,
        SpawnError::SpawnRecursive => SpawnFailure::Recursive,
        SpawnError::BoundaryWriteFailed => SpawnFailure::BoundaryWriteFailed,
        SpawnError::WorktreeFenceNarrowFail => SpawnFailure::FenceFail,
        SpawnError::InvalidResumeTarget { reason } => SpawnFailure::InvalidResumeTarget { reason },
    }
}

#[cfg(test)]
mod tests;
