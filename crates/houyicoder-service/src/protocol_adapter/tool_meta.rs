//! Shared tool-call metadata: the human title a card shows, the category a
//! client renders the call under, and the files the call touches. One
//! definition serves the live tool-call notification and the permission
//! card, so a host shows the same name and jump target for both.

use houyicoder_protocol::frontend::session_update::ToolCallLocation;
use houyicoder_protocol::frontend::session_update::ToolKind;
use serde_json::Value;

/// A one-line title keeps the command short enough for a card row.
const COMMAND_TITLE_MAX: usize = 60;

/// The string field a tool's input carries, if any.
fn input_str<'a>(input: Option<&'a Value>, key: &str) -> Option<&'a str> {
    input?.get(key)?.as_str()
}

/// The target a file-shaped call names: an explicit path field first, then
/// the first line of a command.
fn target(input: Option<&Value>) -> Option<&str> {
    input_str(input, "path")
        .or_else(|| input_str(input, "file_path"))
        .or_else(|| input_str(input, "notebook_path"))
}

fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or(text).trim()
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= COMMAND_TITLE_MAX {
        return text.to_string();
    }
    let kept: String = text.chars().take(COMMAND_TITLE_MAX).collect();
    format!("{kept}...")
}

/// The title a client renders for one tool call.
pub(crate) fn tool_title(tool: &str, input: Option<&Value>) -> String {
    match tool {
        "bash" => match input_str(input, "command") {
            Some(command) => format!("Run: {}", truncate(first_line(command))),
            None => "Run a command".to_string(),
        },
        "write" | "edit" | "multiedit" => match target(input) {
            Some(path) => format!("Edit {path}"),
            None => format!("Edit with {tool}"),
        },
        "read" => match target(input) {
            Some(path) => format!("Read {path}"),
            None => "Read a file".to_string(),
        },
        "grep" | "glob" => match input_str(input, "pattern") {
            Some(pattern) => format!("Search {pattern}"),
            None => "Search".to_string(),
        },
        _ => tool.to_string(),
    }
}

/// The category a client groups one tool call under.
pub(crate) fn tool_kind(tool: &str) -> ToolKind {
    match tool {
        "bash" => ToolKind::Execute,
        "write" | "edit" | "multiedit" => ToolKind::Edit,
        "read" => ToolKind::Read,
        "grep" | "glob" => ToolKind::Search,
        _ => ToolKind::Other,
    }
}

/// The files a call touches, so a client can offer a jump to each one.
pub(crate) fn tool_locations(tool: &str, input: Option<&Value>) -> Vec<ToolCallLocation> {
    if !matches!(tool, "write" | "edit" | "multiedit" | "read") {
        return Vec::new();
    }
    match target(input) {
        Some(path) if !path.is_empty() => vec![ToolCallLocation::new(path)],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests;
