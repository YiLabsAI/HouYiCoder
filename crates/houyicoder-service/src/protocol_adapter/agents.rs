//! Agent-type entries for the /agents pane.

use houyicoder_core::agent::multi_agent::registry::{AgentDefinition, PromptSource};
use houyicoder_protocol::frontend::agents::{AgentEntry, AgentPrompt};

/// Convert the delegatable definitions into the entries the /agents pane
/// renders. Only the fields that decide how the type runs and what it may
/// touch are carried in the reply; the rest of a definition is configuration
/// the pane does not show.
pub(crate) fn agent_entries(defs: Vec<AgentDefinition>) -> Vec<AgentEntry> {
    defs.into_iter()
        .map(|def| AgentEntry {
            subagent_type: def.subagent_type,
            when_to_use: def.when_to_use,
            tools: def.tools,
            disallowed_tools: def.disallowed_tools,
            model: def.model,
            effort: def.effort,
            system_prompt: match def.system_prompt {
                PromptSource::Owned(body) => AgentPrompt::Owned(body),
                PromptSource::InheritParent => AgentPrompt::InheritParent,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests;
