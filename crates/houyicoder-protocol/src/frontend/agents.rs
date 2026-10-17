//! Agent-type listing payload. The /agents command asks the server which
//! sub-agent types the model may delegate to; this is the typed payload the
//! response carries so the TUI renders the list without importing the engine
//! crate.

use serde::{Deserialize, Serialize};

use crate::llm::EffortLevel;

/// One sub-agent type the model may delegate to. Carries the fields that
/// decide how the type runs and what it may touch, so the pane can show the
/// cost and the tool boundary without reading the definition files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    /// The name the model delegates by, and the name the pane lists.
    pub subagent_type: String,
    /// What the type is for, as the definition states it. The model reads this
    /// to choose a type, and the pane shows it beside the name.
    pub when_to_use: String,
    /// The tools the type may call. None means it inherits the caller's set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Tools the type is barred from even though the caller may hold them.
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// Model tier the type runs on. None inherits the caller's model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort the type runs at, when it sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortLevel>,
    /// The child's system instructions, own or inherited.
    pub system_prompt: AgentPrompt,
}

/// Source of a sub-agent's system instructions. Owned carries a
/// self-contained prompt; InheritParent reuses the caller's rendered prompt,
/// which keeps the child's request on the caller's cached prefix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "source", content = "body")]
pub enum AgentPrompt {
    /// The type's own prompt text.
    Owned(String),
    /// No prompt of its own, so the child runs on the caller's.
    InheritParent,
}

#[cfg(test)]
mod tests;
