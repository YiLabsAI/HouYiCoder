//! ToolRegistry and StubTool. The Tool trait lives in the ports crate and the
//! ToolError type in the protocol crate; this module holds the in-engine
//! registry and the test stub.
//!
//! Fail-closed defaults borrowed from the CLI buildTool: every tool defaults
//! to non-concurrent, non-readonly, non-destructive, and not requiring
//! approval unless it explicitly declares otherwise. The capability gate and
//! native sandbox layer on top of the trait later — they wrap a Tool, they do
//! not replace it.
//!
//! Tool errors become tool-result content fed back to the model: a failing
//! tool does not abort the loop. Only unknown tools (registry miss) and
//! approval rejections produce non-content outcomes.

use std::collections::HashMap;
use std::sync::Arc;

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::ToolDef;
use serde_json::Value;

/// A registry of tools available to a run. Produces ToolDefs for the
/// CompletionRequest and dispatches by name. A HashMap by name; the
/// partition-by-safety batch dispatcher layers on later without changing this.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool. Last registration wins on name collision (MCP shadow
    /// defense is a later concern; keeps it simple).
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    /// The tool declarations sent to the model in a CompletionRequest.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .values()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// Look up a tool by name. None means the model called an unknown tool;
    /// the loop returns an unknown-tool error result for it.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// A new registry with the named tools removed, for building a child
    /// agent's tool set from its definition's disallowed list. Match is
    /// case insensitive so a disallowed entry catches casing variants
    /// (the disallowed list is authored data, not a dispatch key).
    pub fn narrow(&self, disallowed: &[String]) -> ToolRegistry {
        let mut out = ToolRegistry::new();
        for (name, tool) in &self.tools {
            if disallowed.iter().any(|d| d.eq_ignore_ascii_case(name)) {
                continue;
            }
            out.tools.insert(name.clone(), Arc::clone(tool));
        }
        out
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// A deterministic echo tool for tests: returns {"echo": <input>}. Read-only,
/// concurrency-safe, approval-free — the safe default for loop tests.
pub struct StubTool {
    name: String,
    description: String,
}

impl StubTool {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            description: "echoes its input back as a tool result".to_string(),
        }
    }
}

impl Tool for StubTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move { Ok(serde_json::json!({ "echo": input })) })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pollster::block_on;

    #[test]
    fn test_tool_is_object_safe() {
        let _boxed: Arc<dyn Tool> = Arc::new(StubTool::new("echo"));
    }

    #[test]
    fn test_registry_dispatches_by_name() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(StubTool::new("echo")));
        let defs = reg.tool_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "echo");
        let tool = reg.get("echo").expect("registered");
        let out =
            block_on(tool.execute(ToolCtx::new("test"), serde_json::json!({"x": 1}))).unwrap();
        assert_eq!(out, serde_json::json!({"echo": {"x": 1}}));
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn test_narrow_drops_disallowed() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(StubTool::new("read")));
        reg.register(Arc::new(StubTool::new("write")));
        reg.register(Arc::new(StubTool::new("agent")));
        let child = reg.narrow(&["write".into(), "Agent".into()]);
        assert!(child.get("read").is_some());
        assert!(child.get("write").is_none(), "write must be dropped");
        assert!(
            child.get("agent").is_none(),
            "agent must be dropped (case-insensitive)"
        );
        assert_eq!(child.len(), 1);
    }

    #[test]
    fn test_fail_closed_defaults() {
        let t = StubTool::new("echo");
        // StubTool overrides all four; verify a bare trait object via a
        // minimal struct shows the fail-closed defaults instead.
        struct Dangerous;
        impl Tool for Dangerous {
            fn name(&self) -> &str {
                "danger"
            }
            fn description(&self) -> &str {
                ""
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({})
            }
            fn execute(&self, _ctx: ToolCtx, _: Value) -> PFut<'_, Result<Value, ToolError>> {
                Box::pin(async { Ok(serde_json::json!({})) })
            }
        }
        let d = Dangerous;
        assert!(!d.is_concurrency_safe());
        assert!(!d.is_read_only());
        assert!(d.is_destructive());
        assert!(!d.requires_approval());
        // StubTool is the safe opposite.
        assert!(t.is_concurrency_safe());
        assert!(t.is_read_only());
        assert!(!t.is_destructive());
        assert!(!t.requires_approval());
    }
}
