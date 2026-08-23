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
}
