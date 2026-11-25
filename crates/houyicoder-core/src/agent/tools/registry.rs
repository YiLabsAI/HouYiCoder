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

/// A tool set that exists outside the static registry and can change while
/// the run is live. External tool servers connect in the background, so the
/// tools they expose cannot be known when the registry is built; this is the
/// point that lets a turn see them without the host waiting at startup.
///
/// Implementations answer from their own snapshot: both calls must be cheap
/// and synchronous, because they run on the turn path.
pub trait ToolSource: Send + Sync {
    /// The declarations of every tool currently available.
    fn tool_defs(&self) -> Vec<ToolDef>;
    /// Resolve one tool by its registered name.
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;
    /// The external server a tool came from, when the source knows one. The
    /// source owns the mapping because it is the only layer that knows how a
    /// server's name became a registered tool name.
    fn tool_server(&self, _tool: &str) -> Option<String> {
        None
    }
    /// Instructions for the model's system prompt from connected servers, if
    /// any, optionally narrowed to the named servers.
    fn instructions_for(&self, _allowed: Option<&[String]>) -> Option<String> {
        None
    }
    /// Every connected server's instructions, when the source carries any.
    fn instructions(&self) -> Option<String> {
        self.instructions_for(None)
    }
}

/// A source with a set of names withheld, so a child agent's registry cannot
/// advertise or dispatch a tool its definition excludes. The inner source
/// stays the single owner of what is connected.
struct FilteredSource {
    inner: Arc<dyn ToolSource>,
    disallowed: Vec<String>,
    /// The servers a child definition mounts. None keeps every server; an
    /// empty list mounts none, so a lightweight child does not carry an
    /// external server's tools and instructions.
    allowed_servers: Option<Vec<String>>,
    /// True for a child limited to reading: only external tools that declare
    /// themselves read-only are exposed.
    read_only_tools: bool,
}

impl FilteredSource {
    /// A restriction matches the registered name or its leaf, so a
    /// definition that lists a built-in name (write, edit) also withholds a
    /// server tool that carries the same leaf. Otherwise the authored
    /// restriction would hold for built-ins only, and a child told not to
    /// write could still write through a server.
    fn allows(&self, name: &str) -> bool {
        let leaf = name.rsplit("__").next().unwrap_or(name);
        if self
            .disallowed
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(name) || entry.eq_ignore_ascii_case(leaf))
        {
            return false;
        }
        if self.read_only_tools && !self.inner.get(name).is_some_and(|tool| tool.is_read_only()) {
            return false;
        }
        let Some(allowed) = &self.allowed_servers else {
            return true;
        };
        self.inner.tool_server(name).is_some_and(|server| {
            allowed
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(&server))
        })
    }
}

impl ToolSource for FilteredSource {
    fn tool_defs(&self) -> Vec<ToolDef> {
        self.inner
            .tool_defs()
            .into_iter()
            .filter(|def| self.allows(&def.name))
            .collect()
    }

    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if self.allows(name) {
            self.inner.get(name)
        } else {
            None
        }
    }

    fn instructions_for(&self, allowed: Option<&[String]>) -> Option<String> {
        let narrowed = match (&self.allowed_servers, allowed) {
            (None, other) => other.map(<[String]>::to_vec),
            (Some(own), None) => Some(own.clone()),
            (Some(own), Some(other)) => Some(
                own.iter()
                    .filter(|server| other.contains(server))
                    .cloned()
                    .collect(),
            ),
        };
        self.inner.instructions_for(narrowed.as_deref())
    }
}

/// A registry of tools available to a run. Produces ToolDefs for the
/// CompletionRequest and dispatches by name. A HashMap by name, plus one
/// optional live source; the partition-by-safety batch dispatcher layers on
/// later without changing this.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    source: Option<Arc<dyn ToolSource>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            source: None,
        }
    }

    /// Attach a live source. A name the static set holds keeps its tool, so
    /// a source can neither shadow a built-in nor replace one.
    pub fn with_source(mut self, source: Arc<dyn ToolSource>) -> Self {
        self.source = Some(source);
        self
    }

    /// Register a tool. Last registration wins on name collision; a caller
    /// adding tools from a second source checks contains first and keeps the
    /// tool that already claimed the name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    /// Whether a tool with this name is already registered.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
            || self
                .source
                .as_ref()
                .is_some_and(|source| source.get(name).is_some())
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
        if let Some(source) = &self.source {
            for def in source.tool_defs() {
                if !self.tools.contains_key(&def.name) {
                    defs.push(def);
                }
            }
        }
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Look up a tool by name. None means the model called an unknown tool;
    /// the loop returns an unknown-tool error result for it.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .get(name)
            .cloned()
            .or_else(|| self.source.as_ref().and_then(|source| source.get(name)))
    }

    /// A new registry with the named tools removed and the live source
    /// narrowed, for building a child agent's tool set from its definition.
    /// Match is case insensitive so a disallowed entry catches casing
    /// variants (the disallowed list is authored data, not a dispatch key).
    /// The source is narrowed the same way, so a restriction on a server
    /// tool holds for both the declarations and the dispatch; a server list
    /// narrows which servers the child mounts, an empty list mounts none,
    /// and a read-only child receives only tools that declare themselves
    /// read-only.
    pub fn narrow(
        &self,
        disallowed: &[String],
        allowed_servers: Option<&[String]>,
        read_only_tools: bool,
    ) -> ToolRegistry {
        let mut out = ToolRegistry::new();
        for (name, tool) in &self.tools {
            if disallowed.iter().any(|d| d.eq_ignore_ascii_case(name)) {
                continue;
            }
            out.tools.insert(name.clone(), Arc::clone(tool));
        }
        out.source = self.source.as_ref().map(|source| {
            Arc::new(FilteredSource {
                inner: Arc::clone(source),
                disallowed: disallowed.to_vec(),
                allowed_servers: allowed_servers.map(<[String]>::to_vec),
                read_only_tools,
            }) as Arc<dyn ToolSource>
        });
        out
    }

    /// Number of statically registered tools. A live source adds tools a turn
    /// may also call, which this count does not include.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry carries no statically registered tool, whatever
    /// its live source offers.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Instructions for the model's system prompt from connected servers, if any.
    pub fn instructions(&self) -> Option<String> {
        self.source.as_ref().and_then(|s| s.instructions())
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
