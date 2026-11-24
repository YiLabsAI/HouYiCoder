//! Tests for the child tool scope: which names a child withholds and which
//! servers it mounts.

use houyicoder_core::agent::multi_agent::registry::{
    AgentBackend, AgentDefinition, IsolationMode, MemoryScope, PromptSource,
};

use super::*;

fn definition(mcp_servers: Vec<String>, disallowed: Vec<String>) -> AgentDefinition {
    AgentDefinition {
        subagent_type: "probe".into(),
        when_to_use: String::new(),
        tools: None,
        disallowed_tools: disallowed,
        model: None,
        effort: None,
        permission_mode: None,
        max_turns: None,
        skills: Vec::new(),
        mcp_servers,
        hooks: Vec::new(),
        initial_prompt: None,
        memory: MemoryScope::default(),
        isolation: IsolationMode::default(),
        omit_project_context: false,
        color: None,
        system_prompt: PromptSource::Owned(String::new()),
        backend: AgentBackend::default(),
    }
}

/// A child withholds its authored names and both host resource tools, and its
/// server list travels to the source filter; an empty list is still a list.
#[test]
fn test_child_scope_names_servers() {
    let (disallowed, servers) = child_tool_scope(&definition(Vec::new(), vec!["write".into()]));
    assert!(disallowed.contains(&"write".to_string()));
    assert!(disallowed.contains(&LIST_RESOURCES_TOOL.to_string()));
    assert!(disallowed.contains(&READ_RESOURCE_TOOL.to_string()));
    assert_eq!(servers, Some(Vec::new()), "an empty list mounts no server");

    let (_, named) = child_tool_scope(&definition(vec!["alpha".into()], Vec::new()));
    assert_eq!(named, Some(vec!["alpha".to_string()]));
}
