//! The adapter between a discovered server tool and the tools this host
//! runs, carrying the registered name, the approval behavior its annotations
//! declare, the call itself, and the bound that keeps one result from
//! crowding the turn.
//!
//! Names are namespaced by server so two servers may expose one tool name.
//! The annotations are read fail-closed. A tool that says nothing about
//! safety is treated as destructive, and only an explicit read-only hint
//! with no destructive one lets a call run without asking.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use houyicoder_api::session::SessionLog;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use houyicoder_core::agent::ToolSource;
use houyicoder_permission::GuardedTool;
use houyicoder_permission::ModeGate;
use houyicoder_protocol::block_ref::BLOCK_BYTES_KEY;
use houyicoder_protocol::block_ref::BLOCK_REF_KEY;
use houyicoder_protocol::block_ref::HINT_KEY;
use houyicoder_protocol::block_ref::PREVIEW_KEY;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::ToolDef;
use rmcp::RoleClient;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Tool as DeclaredTool, ToolAnnotations,
};
use rmcp::service::Peer;
use serde_json::Map;
use serde_json::Value;
use tokio::time::timeout;

use super::manager::McpConnections;
use super::manager::instructions_block;

/// The prefix every server tool carries, so the model can tell the source of
/// a call and the host can refuse a name a server tries to claim outside it.
pub const NAME_PREFIX: &str = "mcp__";

/// The longest registered name. Model gateways cap function names; the
/// namespace plus both parts must fit.
pub const MAX_NAME_CHARS: usize = 64;

/// The largest result returned inline. A larger one is published as a block
/// in the session's store and the model receives a marker that names it.
pub const INLINE_RESULT_BYTES: usize = 8 * 1024 * 1024;

/// How many characters of an externalized result the marker keeps inline, so
/// the model can judge whether it needs the whole block. The cut follows a
/// character boundary, never a byte offset. The transcript's retention
/// preview is shorter because it only labels a row; this one has to carry
/// enough of the result for the model to decide.
pub const PREVIEW_CHARS: usize = 512;

/// The registered name for one server tool: the namespace, the server, and
/// the tool, with every character outside the safe set replaced. A name past
/// the cap keeps its head and a short hash of the whole, so two long names
/// that agree up to the cut stay distinct.
pub fn registered_name(server: &str, tool: &str) -> String {
    fit_name(&sanitized_name(server, tool))
}

/// The name a second tool takes when two tools normalize to the same
/// registered name: the head plus a hash of the raw server and tool pair, so
/// both stay distinct and reachable. Sanitizing many characters to one makes
/// this collision possible (a dot and an underscore both become an
/// underscore), so the suffix hashes the pair before sanitizing.
fn distinguished_name(server: &str, tool: &str) -> String {
    hashed_name(
        &sanitized_name(server, tool),
        &format!("{server}\u{0}{tool}"),
    )
}

fn sanitized_name(server: &str, tool: &str) -> String {
    let sanitize = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("{NAME_PREFIX}{}__{}", sanitize(server), sanitize(tool))
}

fn fit_name(full: &str) -> String {
    if full.chars().count() <= MAX_NAME_CHARS {
        return full.to_string();
    }
    hashed_name(full, full)
}

fn hashed_name(full: &str, identity: &str) -> String {
    let hash = short_hash(identity);
    let keep = MAX_NAME_CHARS - hash.len() - 1;
    let head: String = full.chars().take(keep).collect();
    format!("{head}_{hash}")
}

/// What a tool's annotations say about running it without asking.
pub(crate) struct ToolSafety {
    pub read_only: bool,
    pub destructive: bool,
}

/// Read one tool's hints into the two traits the permission gate consults.
/// Only an explicit read-only hint without a destructive one marks the tool
/// safe. Silence is unknown and reads as destructive.
pub(crate) fn interpret_hints(annotations: Option<&ToolAnnotations>) -> ToolSafety {
    let read_only = annotations
        .and_then(|a| a.read_only_hint)
        .is_some_and(|hint| hint)
        && !annotations
            .and_then(|a| a.destructive_hint)
            .is_some_and(|hint| hint);
    let destructive = annotations
        .and_then(|a| a.destructive_hint)
        .unwrap_or(!read_only);
    ToolSafety {
        read_only,
        destructive,
    }
}

/// Whether one rendered result exceeds what may be returned inline.
pub(crate) fn exceeds_inline(rendered: &str, budget: usize) -> bool {
    rendered.len() > budget
}

/// A stable four-character suffix, so two names sharing a long prefix stay
/// distinct after truncation.
fn short_hash(text: &str) -> String {
    let mut hash: u64 = 1469598103934665603;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("{:04x}", hash as u16)
}

/// One server tool, ready for the registry.
pub(crate) struct McpTool {
    registered: String,
    /// The name the server itself uses, for the call it receives.
    server_name: String,
    description: String,
    schema: Value,
    read_only: bool,
    destructive: bool,
    peer: Peer<RoleClient>,
    call_timeout: Duration,
    log: Option<Arc<dyn SessionLog>>,
    inline_budget: usize,
}

impl McpTool {
    /// Adapt one discovered tool. The annotations decide whether a call runs
    /// without asking, read fail-closed.
    pub(crate) fn adapt(
        server: &str,
        tool: &DeclaredTool,
        peer: Peer<RoleClient>,
        call_timeout: Duration,
        log: Option<Arc<dyn SessionLog>>,
    ) -> Self {
        let ToolSafety {
            read_only,
            destructive,
        } = interpret_hints(tool.annotations.as_ref());
        Self {
            registered: registered_name(server, &tool.name),
            server_name: tool.name.to_string(),
            description: tool
                .description
                .as_deref()
                .unwrap_or("a tool provided by an external server")
                .to_string(),
            schema: Value::Object((*tool.input_schema).clone()),
            read_only,
            destructive,
            peer,
            call_timeout,
            log,
            inline_budget: INLINE_RESULT_BYTES,
        }
    }

    /// Shrink the inline bound, so a test can reach the externalize path
    /// without holding megabytes of output.
    pub(crate) fn with_inline_budget(mut self, budget: usize) -> Self {
        self.inline_budget = budget;
        self
    }

    /// Take a name other than the one this tool adapted to, so a tool whose
    /// normalized name collides with an earlier one stays distinct.
    pub(crate) fn renamed(mut self, name: String) -> Self {
        self.registered = name;
        self
    }
}

/// Publish one oversized payload as a content-addressed block and return the
/// marker the model reads instead. Shared by every producer in this module,
/// so a stored result decodes the same way whichever tool wrote it.
pub(crate) async fn externalize_result(
    log: Option<&Arc<dyn SessionLog>>,
    session: Option<SessionId>,
    payload: Value,
) -> Value {
    let rendered = match &payload {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let preview: String = rendered.chars().take(PREVIEW_CHARS).collect();
    let bytes = serde_json::to_vec(&payload).unwrap_or_else(|_| rendered.into_bytes());
    let length = bytes.len();
    let Some((log, session)) = log.zip(session) else {
        return result_marker(
            None,
            length,
            preview,
            "the result exceeded the inline budget and no session store is bound to hold it; the preview is all that is available",
        );
    };
    match log.backend().block_put(bytes).await {
        Ok(hash) => {
            let hint = format!(
                "the result exceeded the inline budget; its whole text is stored in block {} of session {}",
                hash.0, session
            );
            result_marker(Some(hash.0), length, preview, &hint)
        }
        Err(error) => {
            tracing::warn!(%error, "an oversized tool result could not be stored");
            result_marker(
                None,
                length,
                preview,
                &format!(
                    "the result exceeded the inline budget and could not be stored: {error}; the preview is all that is available"
                ),
            )
        }
    }
}

/// The marker an externalized result is replaced with.
fn result_marker(address: Option<String>, length: usize, preview: String, hint: &str) -> Value {
    let mut marker = Map::new();
    if let Some(address) = address {
        marker.insert(BLOCK_REF_KEY.to_string(), Value::String(address));
    }
    marker.insert(BLOCK_BYTES_KEY.to_string(), Value::from(length));
    marker.insert(PREVIEW_KEY.to_string(), Value::String(preview));
    marker.insert(HINT_KEY.to_string(), Value::String(hint.to_string()));
    Value::Object(marker)
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.registered
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            let arguments = match input {
                Value::Null => None,
                Value::Object(map) => Some(map),
                other => {
                    return Err(ToolError::InvalidInput(format!(
                        "the tool takes an object, not {}",
                        type_name(&other)
                    )));
                }
            };
            let mut params = CallToolRequestParams::new(self.server_name.clone());
            if let Some(arguments) = arguments {
                params = params.with_arguments(arguments);
            }
            let result = timeout(self.call_timeout, self.peer.call_tool(params))
                .await
                .map_err(|_| {
                    ToolError::Failed(format!(
                        "the server did not answer within {}ms",
                        self.call_timeout.as_millis()
                    ))
                })?
                .map_err(|error| ToolError::Failed(error.to_string()))?;
            self.settle(ctx, result).await
        })
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn is_destructive(&self) -> bool {
        self.destructive
    }
}

impl McpTool {
    /// Turn one call result into the payload the model reads, externalizing
    /// it when it exceeds the inline bound.
    async fn settle(&self, ctx: ToolCtx, result: CallToolResult) -> Result<Value, ToolError> {
        let text = text_of(&result);
        if result.is_error == Some(true) {
            let detail = if text.is_empty() {
                result
                    .structured_content
                    .as_ref()
                    .map(Value::to_string)
                    .unwrap_or_else(|| "the server reported an error without details".into())
            } else {
                text
            };
            return Err(ToolError::Failed(detail));
        }
        let payload = match (&result.structured_content, text.is_empty()) {
            (Some(structured), _) => structured.clone(),
            (None, false) => Value::String(text.clone()),
            // A server may answer with an image or a resource and no text.
            // Hand the blocks to the model as they arrived rather than an
            // empty string that says nothing about what the server returned.
            (None, true) if !result.content.is_empty() => {
                serde_json::to_value(&result.content).unwrap_or(Value::Null)
            }
            (None, true) => Value::String(String::new()),
        };
        let rendered = match &payload {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        if exceeds_inline(&rendered, self.inline_budget) {
            return Ok(externalize_result(self.log.as_ref(), ctx.session_id, payload).await);
        }
        Ok(payload)
    }
}

/// The text blocks of one result, joined in order.
fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The live tools the registry consults, held as a snapshot the subscription
/// task keeps current, so a turn reads connected servers without waiting for
/// any handshake.
#[derive(Default)]
pub struct McpToolSource {
    tools: RwLock<BTreeMap<String, Arc<dyn Tool>>>,
    /// The configured server each registered tool came from.
    servers: RwLock<BTreeMap<String, String>>,
    /// Each connected server's raw instructions, kept per server so a child
    /// agent's server list can narrow the block it receives.
    instructions: RwLock<BTreeMap<String, String>>,
}

impl McpToolSource {
    /// Replace the snapshot with the tools currently connected and the
    /// server each of them came from.
    pub(crate) fn replace(
        &self,
        tools: BTreeMap<String, Arc<dyn Tool>>,
        servers: BTreeMap<String, String>,
    ) {
        if let Ok(mut current) = self.tools.write() {
            *current = tools;
        }
        if let Ok(mut current) = self.servers.write() {
            *current = servers;
        }
    }

    /// Replace the per-server instructions from connected servers.
    pub(crate) fn replace_instructions(&self, instructions: BTreeMap<String, String>) {
        if let Ok(mut current) = self.instructions.write() {
            *current = instructions;
        }
    }

    /// The names currently exposed, for a status view or a test.
    pub fn names(&self) -> Vec<String> {
        self.tools
            .read()
            .map(|tools| tools.keys().cloned().collect())
            .unwrap_or_default()
    }
}

impl ToolSource for McpToolSource {
    fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .read()
            .map(|tools| {
                tools
                    .values()
                    .map(|tool| ToolDef {
                        name: tool.name().to_string(),
                        description: tool.description().to_string(),
                        input_schema: tool.input_schema(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .read()
            .ok()
            .and_then(|tools| tools.get(name).cloned())
    }

    fn tool_server(&self, tool: &str) -> Option<String> {
        self.servers.read().ok()?.get(tool).cloned()
    }

    fn instructions_for(&self, allowed: Option<&[String]>) -> Option<String> {
        let sections = self.instructions.read().ok()?;
        instructions_block(&sections, allowed)
    }
}

impl McpConnections {
    /// The tools this manager currently exposes, as one live source the
    /// registry consults. A background task keeps it current: the manager
    /// announces every change to the connected set, and both the adapters
    /// and the combined instructions are rebuilt on each announcement, never
    /// on a timer. The task ends when the manager stops.
    ///
    /// Each adapter is wrapped in the mode gate before it reaches the source.
    /// A server tool is external code, so it must pass the same permission
    /// decision as a built-in one; an unwrapped adapter would hand the runner
    /// a tool whose every call runs without asking.
    pub fn tool_source(
        self: &Arc<Self>,
        gate: Arc<dyn ModeGate>,
        log: Option<Arc<dyn SessionLog>>,
    ) -> Arc<McpToolSource> {
        let source = Arc::new(McpToolSource::default());
        let manager = Arc::clone(self);
        let tracker = Arc::clone(self);
        let shared = Arc::clone(&source);
        let task = tokio::spawn(async move {
            let mut changes = manager.change_receiver();
            let stopping = manager.watch_stop();
            loop {
                let snapshot = manager.tools_snapshot().await;
                let mut adapters: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
                let mut servers: BTreeMap<String, String> = BTreeMap::new();
                for (server, tool) in &snapshot {
                    let Some(peer) = manager.peer_of(server).await else {
                        continue;
                    };
                    let timeout = manager.call_timeout_of(server).await;
                    let adapter = McpTool::adapt(server, tool, peer, timeout, log.clone());
                    let registered = adapter.name().to_string();
                    // Two tools whose raw names differ only in a character
                    // the namespace replaces normalize to one name. Keep
                    // both: the second takes the hashed form so neither
                    // silently replaces the other.
                    let adapter = if adapters.contains_key(&registered) {
                        let distinct = distinguished_name(server, &tool.name);
                        tracing::warn!(
                            server = %server,
                            tool = %tool.name,
                            name = %distinct,
                            "two server tools normalize to one name; the second is exposed under a hashed name"
                        );
                        adapter.renamed(distinct.clone())
                    } else {
                        adapter
                    };
                    let guarded = GuardedTool::new(Arc::new(adapter), Arc::clone(&gate));
                    servers.insert(guarded.name().to_string(), server.clone());
                    adapters.insert(guarded.name().to_string(), Arc::new(guarded));
                }
                let instructions = manager.instruction_sections().await;
                shared.replace(adapters, servers);
                shared.replace_instructions(instructions);
                tokio::select! {
                    changed = changes.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    _ = stopping.cancelled() => return,
                }
            }
        });
        tracker.track(task);
        source
    }
}

#[cfg(test)]
mod tests;
