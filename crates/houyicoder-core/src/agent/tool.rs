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
    /// Ordered deterministically by name so the tools array serialized into
    /// the request body is byte-stable across calls and tool registrations.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        let mut defs: Vec<ToolDef> = self
            .tools
            .values()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
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
mod tests;
