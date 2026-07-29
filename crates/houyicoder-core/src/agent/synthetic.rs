//! Synthetic tool-result outcomes: the dispatcher / control-flow results that
//! are NOT tool execution errors (registry miss, user reject, run interrupt).
//! Each generates the bare model-visible JSON the dispatch path emitted before
//! concentration, with no tool-error prefix (the tool-error ones carry one).
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

/// A synthetic tool-result outcome that is not a tool execution error.
/// Registry misses, user rejections, and run interruptions surface here so
/// the model sees a lossless tool_result; the strings stay bit-equivalent to
/// the bare JSON the dispatch path emitted before concentration.
pub(crate) enum SyntheticToolOutcome {
    /// A tool call whose name is not in the registry. on_resume distinguishes
    /// the resume-path miss (the tool was removed between run and resume) from
    /// the dispatch-path miss.
    UnknownTool { name: String, on_resume: bool },
    /// The user rejected the approval for this tool call.
    Rejected,
    /// The run was interrupted with a tool call pending a result.
    Interrupted,
}

impl SyntheticToolOutcome {
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
            SyntheticToolOutcome::UnknownTool {
                name: "x".into(),
                on_resume: false
            }
            .to_json(),
            serde_json::json!({"error": "unknown tool: x"})
        );
        assert_eq!(
            SyntheticToolOutcome::UnknownTool {
                name: "x".into(),
                on_resume: true
            }
            .to_json(),
            serde_json::json!({"error": "unknown tool on resume"})
        );
        assert_eq!(
            SyntheticToolOutcome::Rejected.to_json(),
            serde_json::json!({"error": "rejected by user"})
        );
        assert_eq!(
            SyntheticToolOutcome::Interrupted.to_json(),
            serde_json::json!({"error": "interrupted by user"})
        );
    }
}
