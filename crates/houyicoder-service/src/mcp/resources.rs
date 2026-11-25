//! The two host tools that reach a connected server's resources: one lists
//! what a server offers, the other reads one resource by address. Both are
//! read-only, so a call runs without a permission card.

use std::sync::Arc;

use houyicoder_api::session::SessionLog;
use houyicoder_api::tool::Tool;
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use rmcp::model::Resource;
use rmcp::model::ResourceContents;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use super::manager::McpConnections;
use super::manager::name_in_scope;
use super::tools::INLINE_RESULT_BYTES;
use super::tools::exceeds_inline;
use super::tools::externalize_result;
use super::tools::type_name;

/// The tool name the model calls to list resources.
pub const LIST_RESOURCES_TOOL: &str = "list_mcp_resources";

/// The tool name the model calls to read one resource.
pub const READ_RESOURCE_TOOL: &str = "read_mcp_resource";

/// Lists the resources connected servers offer, optionally for one server.
pub struct ListMcpResourcesTool {
    manager: Arc<McpConnections>,
    log: Option<Arc<dyn SessionLog>>,
    inline_budget: usize,
    /// The servers a child definition mounts; None keeps every connected
    /// server, an empty list reaches none.
    allowed_servers: Option<Vec<String>>,
}

impl ListMcpResourcesTool {
    /// The tool over one manager, with the session store an oversized
    /// listing is published into.
    pub fn new(manager: Arc<McpConnections>, log: Option<Arc<dyn SessionLog>>) -> Self {
        Self {
            manager,
            log,
            inline_budget: INLINE_RESULT_BYTES,
            allowed_servers: None,
        }
    }

    /// Shrink the inline bound, so a test can reach the externalize path
    /// without holding a huge listing.
    pub(crate) fn with_inline_budget(mut self, budget: usize) -> Self {
        self.inline_budget = budget;
        self
    }

    /// Restrict the listing to the servers a child definition mounts.
    pub(crate) fn with_allowed_servers(mut self, servers: Option<Vec<String>>) -> Self {
        self.allowed_servers = servers;
        self
    }
}

/// Whether a server is inside the tool's scope. None keeps every server, and
/// a name matches ignoring case: the list is authored data, not a dispatch
/// key.
fn scope_allows(allowed: Option<&[String]>, server: &str) -> bool {
    allowed.is_none_or(|list| name_in_scope(list, server))
}

impl Tool for ListMcpResourcesTool {
    fn name(&self) -> &str {
        LIST_RESOURCES_TOOL
    }

    fn description(&self) -> &str {
        "List the resources the connected tool servers offer, optionally for one server"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Only list this server's resources"
                }
            }
        })
    }

    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            let server = match input {
                Value::Null => None,
                Value::Object(ref map) => match map.get("server") {
                    None => None,
                    Some(Value::String(name)) => Some(name.clone()),
                    Some(other) => {
                        return Err(ToolError::InvalidInput(format!(
                            "server must be a string, not {}",
                            type_name(other)
                        )));
                    }
                },
                other => {
                    return Err(ToolError::InvalidInput(format!(
                        "the tool takes an object, not {}",
                        type_name(&other)
                    )));
                }
            };
            if let Some(name) = &server
                && !scope_allows(self.allowed_servers.as_deref(), name)
            {
                return Err(ToolError::InvalidInput(format!(
                    "{name} is not among the servers this agent mounts"
                )));
            }
            let mut resources = self
                .manager
                .resources_for(server.as_deref())
                .await
                .map_err(ToolError::Failed)?;
            resources.retain(|(name, _)| scope_allows(self.allowed_servers.as_deref(), name));
            let payload = render_resources(&resources);
            let rendered = payload.to_string();
            if !exceeds_inline(&rendered, self.inline_budget) {
                return Ok(payload);
            }
            Ok(externalize_result(self.log.as_ref(), ctx.session_id, payload).await)
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_destructive(&self) -> bool {
        false
    }
}

/// Reads one resource from one connected server.
pub struct ReadMcpResourceTool {
    manager: Arc<McpConnections>,
    log: Option<Arc<dyn SessionLog>>,
    inline_budget: usize,
    /// The servers a child definition mounts; None keeps every connected
    /// server, an empty list reaches none.
    allowed_servers: Option<Vec<String>>,
}

impl ReadMcpResourceTool {
    /// The tool over one manager, with the session store a large answer is
    /// published into.
    pub fn new(manager: Arc<McpConnections>, log: Option<Arc<dyn SessionLog>>) -> Self {
        Self {
            manager,
            log,
            inline_budget: INLINE_RESULT_BYTES,
            allowed_servers: None,
        }
    }

    /// Shrink the inline bound, so a test can reach the externalize path
    /// without holding a huge resource.
    pub(crate) fn with_inline_budget(mut self, budget: usize) -> Self {
        self.inline_budget = budget;
        self
    }

    /// Restrict the read to the servers a child definition mounts.
    pub(crate) fn with_allowed_servers(mut self, servers: Option<Vec<String>>) -> Self {
        self.allowed_servers = servers;
        self
    }
}

impl Tool for ReadMcpResourceTool {
    fn name(&self) -> &str {
        READ_RESOURCE_TOOL
    }

    fn description(&self) -> &str {
        "Read one resource from a connected tool server by its address"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {"type": "string", "description": "The server that lists the resource"},
                "uri": {"type": "string", "description": "The resource address the server reported"}
            },
            "required": ["server", "uri"]
        })
    }

    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            let map = match input {
                Value::Null => Map::new(),
                Value::Object(map) => map,
                other => {
                    return Err(ToolError::InvalidInput(format!(
                        "the tool takes an object, not {}",
                        type_name(&other)
                    )));
                }
            };
            let server = map
                .get("server")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidInput("server is required".into()))?;
            let uri = map
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidInput("uri is required".into()))?;
            if !scope_allows(self.allowed_servers.as_deref(), server) {
                return Err(ToolError::InvalidInput(format!(
                    "{server} is not among the servers this agent mounts"
                )));
            }
            let contents = self
                .manager
                .read_resource(server, uri)
                .await
                .map_err(ToolError::Failed)?;
            if contents.is_empty() {
                return Ok(Value::String(
                    "the server returned no content for this address".into(),
                ));
            }
            let text = render_contents(&contents);
            if !exceeds_inline(&text, self.inline_budget) {
                return Ok(Value::String(text));
            }
            Ok(externalize_result(self.log.as_ref(), ctx.session_id, Value::String(text)).await)
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_destructive(&self) -> bool {
        false
    }
}

/// The listing the model reads: one entry per resource, naming its server.
fn render_resources(resources: &[(String, Resource)]) -> Value {
    let rows: Vec<Value> = resources
        .iter()
        .map(|(server, resource)| {
            let mut row = Map::new();
            row.insert("server".into(), Value::String(server.clone()));
            row.insert("uri".into(), Value::String(resource.uri.clone()));
            row.insert("name".into(), Value::String(resource.name.clone()));
            if let Some(description) = &resource.description {
                row.insert("description".into(), Value::String(description.clone()));
            }
            if let Some(mime_type) = &resource.mime_type {
                row.insert("mimeType".into(), Value::String(mime_type.clone()));
            }
            Value::Object(row)
        })
        .collect();
    Value::Array(rows)
}

/// The content the model reads: text blocks joined under their addresses,
/// and a binary block named by size rather than decoded.
fn render_contents(contents: &[ResourceContents]) -> String {
    let mut text = String::new();
    for content in contents {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        match content {
            ResourceContents::TextResourceContents {
                uri: block_uri,
                mime_type,
                text: block_text,
                ..
            } => {
                text.push_str("# ");
                text.push_str(block_uri);
                if let Some(mime) = mime_type {
                    text.push_str(&format!(" ({mime})"));
                }
                text.push('\n');
                text.push_str(block_text);
            }
            ResourceContents::BlobResourceContents {
                uri: block_uri,
                mime_type,
                blob,
                ..
            } => {
                text.push_str("[binary resource ");
                text.push_str(block_uri);
                if let Some(mime) = mime_type {
                    text.push_str(&format!(" {mime}"));
                }
                text.push_str(&format!(": {} encoded bytes]", blob.len()));
            }
            _ => {
                text.push_str("[a resource kind this host does not render]");
            }
        }
    }
    text
}

#[cfg(test)]
mod tests;
