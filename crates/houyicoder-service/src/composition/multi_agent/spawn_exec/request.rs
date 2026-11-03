//! Building the child's spawn request and mapping spawn failures.
//!
//! One builder for both run modes so the foreground and background paths cannot
//! drift apart on what a child carries.

use super::super::last_assistant_text;
use super::*;
use houyicoder_api::session::SessionLog;
use houyicoder_core::agent::multi_agent::PartialTranscript;

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
    let child_config = RunnerConfig {
        instructions: child_system_prompt(
            &base_prompt,
            &this.cwd,
            &this.config.model,
            def.omit_project_context,
        ),
        // The definition's own ceiling wins, so a read-only child cannot spend
        // the parent's whole budget on one question. A definition that names
        // none inherits the parent's, which keeps a hand-written agent from
        // silently getting a zero-turn run.
        max_turns: def.max_turns.unwrap_or(this.config.max_turns),
        ..this.config.clone()
    };
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
        tools: this.tools.narrow(&def.disallowed_tools),
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
