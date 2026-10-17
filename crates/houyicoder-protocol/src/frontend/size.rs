//! The resident byte cost of a frontend payload.
//!
//! A holder that keeps frontend payloads spends its memory on the text and JSON
//! those payloads carry, not on their count, so a bound on what it retains is
//! sized from the payloads. The walk covers every field that can grow without
//! bound: message text, image data, tool input and output, plan entries,
//! advertised commands, session metadata, config options, the metadata a
//! producer attaches to any of them, and a notification's own parameters.
//!
//! Each entry reports the bytes the value's own payload holds. The fixed cost
//! of the slot a value sits in belongs to the type holding it, so the holder
//! charges that cost itself rather than reading it from here. The walk is
//! written without a catch-all branch, so a new content block or update variant
//! stops this module compiling rather than joining a retention budget
//! unmeasured.

use serde_json::Value;

use super::run::ContentBlock;
use super::session_update::{
    AvailableCommand, AvailableCommandsUpdate, ConfigOptionUpdate, ContentChunk, Plan,
    SessionInfoUpdate, SessionUpdate, ToolCall, ToolCallContent, ToolCallUpdate,
};
use crate::acp_wire::Meta;
use crate::acpx::AcpxNotification;

/// Cost of a JSON value that carries no string of its own, covering the digits
/// or keyword plus the separator around it.
const JSON_SCALAR_BYTES: usize = 8;

impl SessionUpdate {
    /// The bytes this update's own payload holds, excluding the fixed cost of
    /// the slot holding it. The holder charges its own cost.
    pub fn estimated_bytes(&self) -> usize {
        match self {
            SessionUpdate::UserMessageChunk(chunk)
            | SessionUpdate::AgentMessageChunk(chunk)
            | SessionUpdate::AgentThoughtChunk(chunk) => chunk_bytes(chunk),
            SessionUpdate::ToolCall(call) => tool_call_bytes(call),
            SessionUpdate::ToolCallUpdate(update) => tool_update_bytes(update),
            SessionUpdate::Plan(plan) => plan_bytes(plan),
            SessionUpdate::AvailableCommandsUpdate(update) => commands_bytes(update),
            SessionUpdate::CurrentModeUpdate(update) => {
                update.current_mode_id.len() + meta_bytes(update.meta.as_ref())
            }
            SessionUpdate::ConfigOptionUpdate(update) => options_bytes(update),
            SessionUpdate::SessionInfoUpdate(update) => info_bytes(update),
            SessionUpdate::UsageUpdate(update) => meta_bytes(update.meta.as_ref()),
        }
    }
}

impl AcpxNotification {
    /// The bytes this notification's own payload holds, excluding the fixed
    /// cost of the slot holding it. Its fields travel as JSON, so the walk
    /// measures the parameters it decoded into.
    pub fn estimated_bytes(&self) -> usize {
        json_bytes(&self.params)
    }
}

/// The bytes of one rendered turn chunk. An image carries its encoded data
/// inline, so it can outweigh every text chunk in a retained window.
fn chunk_bytes(chunk: &ContentChunk) -> usize {
    content_block_bytes(&chunk.content)
}

fn content_block_bytes(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.len(),
        ContentBlock::Image { data, mime_type } => data.len() + mime_type.len(),
    }
}

/// The bytes of one tool call. The arguments and the output dominate, since an
/// edit or a command carries far more text than its title does.
fn tool_call_bytes(call: &ToolCall) -> usize {
    call.tool_call_id.0.len()
        + call.title.len()
        + call.content.iter().map(tool_content_bytes).sum::<usize>()
        + call.locations.iter().map(|at| at.path.len()).sum::<usize>()
        + call.raw_input.as_ref().map_or(0, json_bytes)
        + call.raw_output.as_ref().map_or(0, json_bytes)
}

fn tool_content_bytes(content: &ToolCallContent) -> usize {
    match content {
        ToolCallContent::Content { content } => content_block_bytes(content),
    }
}

/// The bytes of a tool call update, which carries the result the call
/// produced: what a command printed, or the file it read.
fn tool_update_bytes(update: &ToolCallUpdate) -> usize {
    update.tool_call_id.0.len()
        + update.fields.title.as_ref().map_or(0, String::len)
        + update.fields.raw_input.as_ref().map_or(0, json_bytes)
        + update.fields.raw_output.as_ref().map_or(0, json_bytes)
}

/// The bytes of a plan, whose entries carry the step text a reader sees.
fn plan_bytes(plan: &Plan) -> usize {
    plan.entries
        .iter()
        .map(|entry| entry.content.len() + meta_bytes(entry.meta.as_ref()))
        .sum::<usize>()
        + meta_bytes(plan.meta.as_ref())
}

fn commands_bytes(update: &AvailableCommandsUpdate) -> usize {
    update
        .available_commands
        .iter()
        .map(command_bytes)
        .sum::<usize>()
        + meta_bytes(update.meta.as_ref())
}

/// The bytes of a config-option change. Each option's value is opaque JSON, so
/// the walk measures whatever the producer put in it rather than nothing.
fn options_bytes(update: &ConfigOptionUpdate) -> usize {
    update.config_options.iter().map(json_bytes).sum::<usize>() + meta_bytes(update.meta.as_ref())
}

/// The bytes of a session metadata change: the title a frontend displays and
/// its timestamp, either of which can carry long text.
fn info_bytes(update: &SessionInfoUpdate) -> usize {
    update.title.as_ref().map_or(0, String::len)
        + update.updated_at.as_ref().map_or(0, String::len)
        + meta_bytes(update.meta.as_ref())
}

fn command_bytes(command: &AvailableCommand) -> usize {
    command.name.len()
        + command.description.len()
        + command.input.as_ref().map_or(0, |input| {
            input.hint.len() + meta_bytes(input.meta.as_ref())
        })
        + meta_bytes(command.meta.as_ref())
}

fn meta_bytes(meta: Option<&Meta>) -> usize {
    meta.map_or(0, |fields| {
        fields
            .iter()
            .map(|(key, value)| key.len() + json_bytes(value))
            .sum()
    })
}

/// The byte cost of a JSON payload, sized by walking it. Serializing would
/// allocate a throwaway string per measurement and measure the encoded form;
/// the walk measures what is held.
fn json_bytes(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => JSON_SCALAR_BYTES,
        Value::String(text) => text.len() + 2,
        Value::Array(items) => 2 + items.iter().map(json_bytes).sum::<usize>(),
        Value::Object(entries) => {
            2 + entries
                .iter()
                .map(|(key, value)| key.len() + 3 + json_bytes(value))
                .sum::<usize>()
        }
    }
}

#[cfg(test)]
mod tests;
