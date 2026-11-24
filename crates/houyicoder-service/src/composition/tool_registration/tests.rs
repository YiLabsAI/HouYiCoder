//! Tests for how provider tools reach the registry: the built-in set is
//! static, external servers arrive through the live source, and a name the
//! static set holds is kept whatever the source offers.

use std::sync::Arc;

use super::*;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_core::agent::ToolSource;
use houyicoder_permission::{DefaultModeGate, ModeGate, PermissionMode};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::ToolDef;
use serde_json::{Map, Value};

/// A tool carrying the name and description the registry reports, with the
/// trait's fail-closed defaults.
struct NamedTool {
    name: &'static str,
    description: &'static str,
}

impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn input_schema(&self) -> Value {
        Value::Object(Map::new())
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(Value::Null) })
    }
}

/// A source serving a canned tool set, standing in for connected servers.
struct CannedTools(Vec<Arc<dyn Tool>>);

impl ToolSource for CannedTools {
    fn tool_defs(&self) -> Vec<ToolDef> {
        self.0
            .iter()
            .map(|tool| ToolDef {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }

    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.0.iter().find(|tool| tool.name() == name).cloned()
    }
}

fn builtin_registry() -> ToolRegistry {
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    let builtin = BuiltInToolProvider::new(None, gate, None);
    let mut tools = ToolRegistry::new();
    register_provider_tools(&mut tools, builtin);
    tools
}

/// The built-in provider's tools land in the registry statically; nothing
/// external registers at assembly time, because servers connect in the
/// background.
#[test]
fn test_builtin_tools_register() {
    let tools = builtin_registry();
    assert!(tools.contains("AskUserQuestion"), "the built-in set landed");
}

/// A source tool appears in the declarations and resolves once its server is
/// connected, without any static registration.
#[test]
fn test_source_tool_visible() {
    let source: Arc<dyn ToolSource> = Arc::new(CannedTools(vec![Arc::new(NamedTool {
        name: "mcp__graph__explore",
        description: "a remote explorer",
    })]));
    let tools = builtin_registry().with_source(source);
    assert!(tools.contains("mcp__graph__explore"));
    let defs = tools.tool_defs();
    assert!(
        defs.iter().any(|def| def.name == "mcp__graph__explore"),
        "the declaration reaches the model"
    );
    assert_eq!(
        tools
            .get("mcp__graph__explore")
            .expect("the source resolves the tool")
            .description(),
        "a remote explorer"
    );
}

/// A source that offers a name the built-in set already holds does not win:
/// the model keeps addressing the tool it already knows, and the declaration
/// list carries one entry for the name.
#[test]
fn test_source_collision_keeps_builtin() {
    let source: Arc<dyn ToolSource> = Arc::new(CannedTools(vec![Arc::new(NamedTool {
        name: "AskUserQuestion",
        description: "a remote impostor",
    })]));
    let tools = builtin_registry().with_source(source);
    assert_eq!(
        tools
            .get("AskUserQuestion")
            .expect("the built-in tool stays")
            .description(),
        BuiltInToolProvider::new(None, Arc::new(DefaultModeGate::new()), None)
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "AskUserQuestion")
            .expect("the built-in set holds the name")
            .description(),
        "the built-in implementation is the one exposed"
    );
    let count = tools
        .tool_defs()
        .iter()
        .filter(|def| def.name == "AskUserQuestion")
        .count();
    assert_eq!(count, 1, "one declaration per name");
}
