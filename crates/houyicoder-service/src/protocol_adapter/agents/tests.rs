//! Tests for this module.

use super::*;
use houyicoder_core::agent::multi_agent::registry::{IsolationMode, MemoryScope};
use houyicoder_protocol::llm::EffortLevel;

fn definition() -> AgentDefinition {
    AgentDefinition {
        subagent_type: "plan".into(),
        when_to_use: "designs a plan".into(),
        tools: None,
        disallowed_tools: vec!["write".into()],
        model: Some("Max".into()),
        effort: Some(EffortLevel::High),
        permission_mode: Some("manual".into()),
        max_turns: Some(3),
        skills: vec!["commit".into()],
        mcp_servers: vec!["docs".into()],
        hooks: vec!["PreToolUse".into()],
        initial_prompt: Some("start here".into()),
        memory: MemoryScope::Disabled,
        isolation: IsolationMode::None,
        omit_project_context: true,
        color: Some("red".into()),
        system_prompt: PromptSource::Owned("you are an architect".into()),
    }
}

/// The entry carries the fields the pane shows and drops the rest of the
/// definition, so the pane's rows and the prompt paragraph describe one type
/// from one source.
#[test]
fn test_entry_carries_shown_fields() {
    let entries = agent_entries(vec![definition()]);
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.subagent_type, "plan");
    assert_eq!(e.when_to_use, "designs a plan");
    assert_eq!(e.model.as_deref(), Some("Max"));
    assert_eq!(e.effort, Some(EffortLevel::High));
    assert_eq!(e.disallowed_tools, vec!["write".to_string()]);
    assert!(e.tools.is_none(), "an inherited tool set stays absent");
    assert_eq!(
        e.system_prompt,
        AgentPrompt::Owned("you are an architect".into())
    );
}

/// An inherited prompt crosses as its own variant, so the pane can say the
/// child reuses the caller's prompt instead of showing an empty body.
#[test]
fn test_inherited_prompt_variant() {
    let mut def = definition();
    def.system_prompt = PromptSource::InheritParent;
    let entries = agent_entries(vec![def]);
    assert_eq!(entries[0].system_prompt, AgentPrompt::InheritParent);
}

#[test]
fn test_empty_input_no_entries() {
    assert!(agent_entries(Vec::new()).is_empty());
}
