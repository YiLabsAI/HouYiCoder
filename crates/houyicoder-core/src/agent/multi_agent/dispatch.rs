//! Runner wiring for agent-tool dispatch: the spawn port and agent identity
//! the per-call ToolCtx carries. Split from the runner module root to stay
//! under the file-size gate.

use std::sync::Arc;

use houyicoder_api::spawn::{AgentIdentity, SpawnHandle};

use crate::agent::Runner;

impl Runner {
    /// Attach the spawn port the agent tool launches children through.
    pub fn with_spawn_handle(mut self, handle: Arc<dyn SpawnHandle>) -> Self {
        self.spawn_handle = Some(handle);
        self
    }

    /// Set the running agent's identity (depth 0 for a top-level runner;
    /// spawn_child sets depth + 1 and the subagent type on children).
    pub fn with_agent_identity(mut self, identity: AgentIdentity) -> Self {
        self.agent_identity = identity;
        self
    }

    /// The identity dispatches carry. pub(crate) for the dispatch site.
    pub(crate) fn agent_identity(&self) -> &AgentIdentity {
        &self.agent_identity
    }

    /// The spawn port dispatches carry, if one is attached.
    pub(crate) fn spawn_handle(&self) -> Option<&Arc<dyn SpawnHandle>> {
        self.spawn_handle.as_ref()
    }

    /// Route a steering text into a running child's inbox (the teammate-view
    /// input path). Delegates to the spawn handle's bus; Err when no
    /// multi-agent runtime is attached or the child has no inbox.
    pub fn steer_child(&self, child_id: &str, text: String) -> Result<(), String> {
        match self.spawn_handle.as_ref() {
            Some(h) => h.send_to_child_inbox(child_id, text),
            None => Err("no spawn handle wired".into()),
        }
    }

    /// Abort the viewed child's current turn without killing the run (the
    /// teammate-view Esc path). Delegates to the spawn handle's child
    /// registry; returns false when no runtime is attached or the child is
    /// not in a live turn. Non-terminal: the child's drive loop appends an
    /// interrupt marker + starts the next turn.
    pub fn cancel_child(&self, child_id: &str) -> bool {
        match self.spawn_handle.as_ref() {
            Some(h) => h.cancel_child_turn(child_id),
            None => false,
        }
    }

    /// Kill a background (async) child: cancel its lifecycle token so the
    /// drive loop returns Interrupted (terminal), distinct from
    /// cancel_child which only aborts the current turn (non-terminal).
    /// Delegates to the spawn handle's child registry; returns false when
    /// no runtime is attached or the child is no longer live (dropped).
    pub fn kill_child(&self, child_id: &str) -> bool {
        match self.spawn_handle.as_ref() {
            Some(h) => h.kill_child(child_id),
            None => false,
        }
    }

    /// Install the agent directory section the system prompt carries so the
    /// model can discover sub-agent types. Set once at the composition root.
    pub fn set_agent_directory(&self, section: String) {
        self.context_builder.set_agent_directory(section);
    }

    /// The agent directory section, if one was installed. Read by the server
    /// when the TUI's /agents command queries the registered types. This is
    /// the exact prompt paragraph the system prompt carries: its bytes are
    /// load-bearing for the prompt cache, so the /agents panel renders it
    /// verbatim - reformat in the TUI, never mutate what this returns.
    pub fn agent_directory(&self) -> Option<String> {
        self.context_builder.agent_directory()
    }
}
