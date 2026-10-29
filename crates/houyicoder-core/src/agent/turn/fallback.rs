//! Fallback tool-result outcomes: the results the runner produces for itself
//! when a call does not run (registry miss, user reject, run interrupt). They
//! are not tool execution errors. Each carries the bare model-visible JSON the
//! model sees, with no tool-error prefix (a real execution error carries one).
//! Kept in the engine core: these are dispatch / control concerns, not wire
//! types, so they do not enter ports or protocol.

use houyicoder_protocol::extension::ToolError;
use serde_json::Value;

/// The model-visible JSON for a real tool execution error. The Display prefix
/// is part of the payload the model sees, so the e.to_string() value is the
/// error field verbatim.
pub(crate) fn tool_error_json(e: &ToolError) -> Value {
    serde_json::json!({ "error": e.to_string() })
}

/// A tool-result outcome the runner produces for a call that did not run.
/// Registry misses, user rejections, and run interruptions surface here so
/// the model sees a lossless tool_result for every call it emitted.
pub(crate) enum FallbackToolOutcome {
    /// A tool call whose name is not in the registry. on_resume distinguishes
    /// the resume-path miss (the tool was removed between run and resume) from
    /// the dispatch-path miss.
    UnknownTool { name: String, on_resume: bool },
    /// The user rejected the approval for this tool call.
    Rejected,
    /// The run was interrupted with a tool call pending a result.
    Interrupted,
}

impl FallbackToolOutcome {
    /// The model-visible tool_result payload for this outcome.
    ///
    /// Every outcome carries the not-run marker: the reason text explains why
    /// the call never executed, and the marker keeps a reader from counting
    /// that text as a verdict on an execution that did not happen.
    pub(crate) fn to_json(&self) -> Value {
        let error = match self {
            Self::UnknownTool {
                on_resume: true, ..
            } => "unknown tool on resume".to_string(),
            Self::UnknownTool {
                name,
                on_resume: false,
            } => format!("unknown tool: {name}"),
            Self::Rejected => "rejected by user".to_string(),
            Self::Interrupted => "interrupted by user".to_string(),
        };
        serde_json::json!({ "error": error, "not_run": true })
    }
}

#[cfg(test)]
mod tests;
