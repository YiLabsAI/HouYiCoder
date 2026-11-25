//! Tests for the child tool scope: which names a child withholds and which
//! servers it mounts.

use houyicoder_core::agent::multi_agent::registry::{
    AgentBackend, AgentDefinition, IsolationMode, MemoryScope, PromptSource,
};

use super::*;

fn definition(mcp_servers: Option<Vec<String>>, disallowed: Vec<String>) -> AgentDefinition {
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

/// A child keeps its authored names, inherits the parent's servers when the
/// definition names none, mounts only a named list, and reads only when it
/// withholds the writing tools.
#[test]
fn test_child_scope_names_servers() {
    let (disallowed, servers, read_only) =
        child_tool_scope(&definition(None, vec!["write".to_string()]));
    assert!(disallowed.contains(&"write".to_string()));
    assert_eq!(servers, None, "an absent list inherits the parent servers");
    assert!(read_only, "a child that cannot write reads only");

    let (_, empty, _) = child_tool_scope(&definition(Some(Vec::new()), Vec::new()));
    assert_eq!(
        empty,
        Some(Vec::new()),
        "an explicit empty list mounts none"
    );

    let (_, named, read_only) =
        child_tool_scope(&definition(Some(vec!["alpha".to_string()]), Vec::new()));
    assert_eq!(named, Some(vec!["alpha".to_string()]));
    assert!(!read_only, "a child without write restrictions may write");
}
