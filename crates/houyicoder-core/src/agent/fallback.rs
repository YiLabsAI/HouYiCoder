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
    /// The model-visible tool_result payload for this outcome. No tool-error
    /// prefix: these are not tool errors.
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::UnknownTool {
                on_resume: true, ..
            } => serde_json::json!({ "error": "unknown tool on resume" }),
            Self::UnknownTool {
                name,
                on_resume: false,
            } => {
                serde_json::json!({ "error": format!("unknown tool: {name}") })
            }
            Self::Rejected => serde_json::json!({ "error": "rejected by user" }),
            Self::Interrupted => serde_json::json!({ "error": "interrupted by user" }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_error_carries_prefix() {
        let e = ToolError::Failed("boom".into());
        let v = tool_error_json(&e);
        assert_eq!(v, serde_json::json!({"error": "tool error: boom"}));
    }

    #[test]
    fn test_outcomes_have_no_prefix() {
        assert_eq!(
            FallbackToolOutcome::UnknownTool {
                name: "x".into(),
                on_resume: false
            }
            .to_json(),
            serde_json::json!({"error": "unknown tool: x"})
        );
        assert_eq!(
            FallbackToolOutcome::UnknownTool {
                name: "x".into(),
                on_resume: true
            }
            .to_json(),
            serde_json::json!({"error": "unknown tool on resume"})
        );
        assert_eq!(
            FallbackToolOutcome::Rejected.to_json(),
            serde_json::json!({"error": "rejected by user"})
        );
        assert_eq!(
            FallbackToolOutcome::Interrupted.to_json(),
            serde_json::json!({"error": "interrupted by user"})
        );
    }
}
