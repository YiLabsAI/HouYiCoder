//! The rows a tool call and its result become: the body a result shows for
//! its output, and the row that stands in for a call whose frames have left
//! the view. Which of these rows a window places, and where, is the walk's
//! business; the builders here only say what a row holds.

use serde_json::Value;

use crate::brief::{MEMORY_LABEL_TOOLS, result_summary, tool_user_facing_name};
use crate::records::{ToolOutcome, TranscriptLine};
use crate::result_body::{
    command_is_silent_success, extract_body, output_has_diff, write_result_body,
};

/// Whether the transcript draws this tool call as a chip. A transparent tool
/// draws through its own widget (the checklist) or as the question prompt
/// itself, so the turn summary counts only the calls the user sees.
pub(super) fn tool_renders_chip(title: &str) -> bool {
    title != "todo_write" && title != "AskUserQuestion"
}

/// Build the single result row for a tool call from its output. The body is
/// tool-specific: Read shows a one-line summary (content stays in the frame
/// log for the model, never the transcript — dumping it flooded the view and
/// repeated the content the model had already read); Bash shows raw stdout (its
/// summary is the first stdout line, which the raw body already starts with, so
/// prepending it duplicates line 1); a silent Bash success renders "done"
/// rather than an empty "(no output)" placeholder that reads as failure; the
/// memory tools render a single human label (the raw JSON is not a readable
/// body); Write renders the chip plus the written content pulled from the
/// call's input (folding is the render layer's job via tool_rows, not baked
/// in). Other tools keep summary + raw when both are present.
pub(super) fn tool_result_line(
    id: &str,
    tool_name: &str,
    output: &Value,
    call_input: Option<&Value>,
) -> TranscriptLine {
    let out_str = output.to_string();
    let raw = extract_body(&out_str);
    let body = if tool_name == "read" {
        if output.get("error").is_some() {
            raw
        } else {
            result_summary(tool_name, output).unwrap_or_default()
        }
    } else if tool_name == "bash" {
        if raw.is_empty() && command_is_silent_success(call_input, output) {
            "done".to_string()
        } else {
            raw
        }
    } else if MEMORY_LABEL_TOOLS.contains(&tool_name) {
        result_summary(tool_name, output).unwrap_or(raw)
    } else if tool_name == "write" {
        write_result_body(output, call_input)
    } else {
        match result_summary(tool_name, output) {
            Some(s) if raw.is_empty() => s,
            Some(s) => format!("{s}\n{raw}"),
            None => raw,
        }
    };
    TranscriptLine::Tool {
        name: "result".to_string(),
        tool: tool_name.to_string(),
        status: String::new(),
        invocation: String::new(),
        outcome: ToolOutcome::from_output_with(
            output,
            tool_name,
            call_input.unwrap_or(&Value::Null),
        ),
        call_id: id.to_string(),
        body,
        is_diff: output_has_diff(&out_str),
    }
}

/// What a call row says when it names the call whose frames left the view: the
/// arguments and the brief were theirs, and they are still in the session log,
/// so the row says where the content went instead of showing arguments it no
/// longer holds.
const CALL_OUT_OF_VIEW: &str = "earlier content is out of view — scroll up to load";

/// What that row says when the state no longer holds the call's name: it can
/// only say a call is missing from above the result, not which one. The bound
/// on the names the state keeps is what puts a row here, and the row is still
/// the record that a call came before its result.
const CALL_OUT_OF_VIEW_UNNAMED: &str = "an earlier call is out of view — scroll up to load";

/// The row standing in for a tool call whose frames left the view, for the
/// result that arrived under it. A result with no call above it reads as a
/// stray line, so the call keeps a row of its own there, named by the state the
/// earlier read handed on. A call the state can no longer name — it holds a
/// bounded number of them — still gets a row, unnamed, rather than a name taken
/// from another call. Past the bound the row appears even for a tool that draws
/// none when its frames are in view: the state no longer knows which tool the
/// call was, and a row is the honest shape for a call it cannot name.
pub(super) fn call_out_of_view_row(
    call_id: &str,
    tool: Option<&str>,
    output: &Value,
) -> TranscriptLine {
    let (name, tool, status) = match tool {
        Some(tool) => (
            tool_user_facing_name(tool, &Value::Null),
            tool,
            CALL_OUT_OF_VIEW,
        ),
        None => ("call", "", CALL_OUT_OF_VIEW_UNNAMED),
    };
    TranscriptLine::Tool {
        name: name.to_string(),
        tool: tool.to_string(),
        status: status.to_string(),
        invocation: String::new(),
        outcome: ToolOutcome::from_output_with(output, tool, &Value::Null),
        call_id: call_id.to_string(),
        body: String::new(),
        is_diff: false,
    }
}
