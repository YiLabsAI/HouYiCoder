//! What the status and instruction views read from the shared slots: one
//! server's state as a view renders it, the diagnostics it wrote, and the
//! instruction block a prompt carries. None of it changes a connection.

use std::collections::BTreeMap;

use houyicoder_protocol::mcp::McpConnectionState;
use houyicoder_protocol::mcp::McpServerDetailView;
use houyicoder_protocol::mcp::McpServerStatusView;
use houyicoder_protocol::mcp::McpToolSummary;

use super::McpConnections;
use super::STDERR_TAIL_LINES;
use crate::mcp::tools::interpret_hints;

/// How many characters of a server's instructions the detail preview carries.
/// The pane renders a bounded window; the prompt block carries the full text.
const INSTRUCTIONS_PREVIEW_CHARS: usize = 480;

/// How many characters of one tool's description the detail carries. The
/// tool's full description stays with the tool definition the model reads.
const TOOL_DESCRIPTION_CHARS: usize = 100;

impl McpConnections {
    /// The servers as the status views render them, ordered by name.
    pub async fn statuses(&self) -> Vec<McpServerStatusView> {
        let slots = self.slots.read().await;
        slots
            .values()
            .map(|slot| McpServerStatusView {
                name: slot.config.name.clone(),
                scope: slot.config.scope,
                state: slot.state.clone(),
                server_version: slot.server_version.clone(),
                tool_count: slot.tools.len(),
                resource_count: slot.resources.len(),
                has_instructions: slot
                    .instructions
                    .as_deref()
                    .is_some_and(|text| !text.is_empty()),
                recent_stderr: slot.stderr.lines(),
                detail: detail_view(slot),
            })
            .collect()
    }

    /// The most recent diagnostic lines one server wrote, oldest first.
    pub async fn stderr_tail(&self, name: &str) -> Vec<String> {
        let slots = self.slots.read().await;
        slots
            .get(name)
            .map(|slot| slot.stderr.lines())
            .unwrap_or_default()
    }

    /// The diagnostics hold size the tail keeps, exposed so a status view
    /// can say how much of the child's output it is eliding.
    pub fn stderr_limit(&self) -> usize {
        STDERR_TAIL_LINES
    }

    /// Read the instructions exported by connected servers, sorted by server
    /// name, as one block of explicit server tags for the prompt. A server
    /// the allowed list excludes contributes neither its instructions nor a
    /// tag.
    pub async fn instructions_for(&self, allowed_servers: Option<&[String]>) -> Option<String> {
        let sections = self.instruction_sections().await;
        instructions_block(&sections, allowed_servers)
    }

    /// The raw instructions each connected server exported, ordered by
    /// server name and trimmed to the per-server character cap.
    pub(crate) async fn instruction_sections(&self) -> BTreeMap<String, String> {
        const MAX_MCP_INSTRUCTIONS_CHARS: usize = 4_096;
        let slots = self.slots.read().await;
        let mut sections = BTreeMap::new();
        for (name, slot) in slots.iter() {
            if !matches!(slot.state, McpConnectionState::Connected) {
                continue;
            }
            if let Some(instructions) = slot
                .instructions
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                let trimmed = instructions.trim();
                let bounded: String = if trimmed.chars().count() > MAX_MCP_INSTRUCTIONS_CHARS {
                    trimmed.chars().take(MAX_MCP_INSTRUCTIONS_CHARS).collect()
                } else {
                    trimmed.to_string()
                };
                sections.insert(name.clone(), bounded);
            }
        }
        sections
    }
}

/// Build one slot's detail page view. Header values are dropped on this
/// side of the boundary: only the names cross, because anything that
/// renders a value is a place a configured secret can leak from.
fn detail_view(slot: &super::ServerSlot) -> McpServerDetailView {
    let mut tools = Vec::with_capacity(slot.tools.len());
    for tool in &slot.tools {
        tools.push(McpToolSummary {
            name: tool.name.to_string(),
            read_only: interpret_hints(tool.annotations.as_ref()).read_only,
            description: bounded(
                tool.description.as_deref().unwrap_or_default(),
                TOOL_DESCRIPTION_CHARS,
            ),
        });
    }
    McpServerDetailView {
        endpoint: slot.config.transport.endpoint.clone(),
        header_names: slot.config.transport.headers.keys().cloned().collect(),
        tools,
        instructions_preview: slot
            .instructions
            .as_deref()
            .map(|text| bounded(text, INSTRUCTIONS_PREVIEW_CHARS)),
        untrusted_lock: slot.locked,
    }
}

/// The first budget characters of a text, with an ellipsis when more
/// follows. Character-count, so a multi-byte rune is never split.
fn bounded(text: &str, budget: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= budget {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(budget.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// One prompt block of server tags from the raw sections. Server text is
/// untrusted, so each entry is wrapped in a tag naming its server and both
/// the text and the name are escaped: server content cannot close the host's
/// frame or leave the attribute.
pub(crate) fn instructions_block(
    sections: &BTreeMap<String, String>,
    allowed_servers: Option<&[String]>,
) -> Option<String> {
    let mut blocks = Vec::new();
    for (name, text) in sections {
        if let Some(allowed) = allowed_servers
            && !name_in_scope(allowed, name)
        {
            continue;
        }
        blocks.push(format!(
            "<mcp_server_instructions server=\"{}\">\n{}\n</mcp_server_instructions>",
            escape_attribute(name),
            escape_text(text)
        ));
    }
    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join("\n\n"))
    }
}

/// Whether a configured server is inside a caller's list. The list is
/// authored data, not a dispatch key, so the match ignores case everywhere
/// one is consulted: tools, resources, and the instruction block agree on
/// which servers a list names.
pub(crate) fn name_in_scope(allowed: &[String], server: &str) -> bool {
    allowed
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(server))
}

/// Escape the characters that would let server text close the host's tag.
fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape a server name for use inside a double-quoted attribute.
fn escape_attribute(name: &str) -> String {
    escape_text(name).replace('"', "&quot;")
}
