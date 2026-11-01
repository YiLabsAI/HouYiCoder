//! Fallback tool-result outcomes: the results the runner produces for itself
//! when a call does not run (registry miss, user reject, run interrupt, check
//! withheld behind an edit that did not complete). They are not tool execution
//! errors. Each carries the bare model-visible JSON the model sees, with no
//! tool-error prefix (a real execution error carries one). Kept in the engine
//! core: they describe what dispatch decided, not a protocol message, so no
//! port or protocol type holds them.

use houyicoder_protocol::extension::ToolError;
use serde_json::Value;

/// Why a shell check did not run: the file edit it followed had not completed
/// when the check was due, so running it would have read source that was not
/// written yet. The edit may be settled as a failure or still await a
/// decision; the model resends the check once it lands.
pub(crate) const SKIPPED_AFTER_EDIT: &str = "skipped: the file edit before it had not completed";

/// The model-visible JSON for a real tool execution error. The Display prefix
/// is part of the payload the model sees, so the e.to_string() value is the
/// error field verbatim.
pub(crate) fn tool_error_json(e: &ToolError) -> Value {
    serde_json::json!({ "error": e.to_string() })
}

/// Who decided a tool call must not run.
#[derive(Clone, Copy)]
pub(crate) enum RefusalBy {
    /// The user declined the approval card.
    User,
    /// A hook fired for the call returned a blocking verdict.
    Hook,
}

/// The blocking verdict a refusal came from. The verdict is carried beside
/// the coarse source so a reader of the durable log can tell a policy
/// refusal from a self-correction note, and the model can tell that the user
/// did not reject the call.
#[derive(Clone, Copy)]
pub(crate) enum RefusalVerdict {
    /// The user answered the approval card with no.
    Reject,
    /// A deny hook refused the call.
    Policy,
    /// A feedback hook asked the model to adjust the call.
    Feedback,
    /// An ask hook escalated the call, which cannot run before an answer.
    Ask,
}

impl RefusalBy {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Hook => "hook",
        }
    }
}

impl RefusalVerdict {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Policy => "policy",
            Self::Feedback => "feedback",
            Self::Ask => "ask",
        }
    }
}

/// The model-visible result for a call that was refused before it ran.
///
/// The payload carries three facts: the reason text, the not-run marker (the
/// call never executed, so the outcome counts cannot read it as a failed
/// run), and a refused object naming who refused the call and from which
/// verdict. The reason text stays the leading explanation the model reads;
/// the markers are what the consumers separate it by.
pub(crate) fn refused_json(message: String, by: RefusalBy, verdict: RefusalVerdict) -> Value {
    serde_json::json!({
        "error": message,
        "not_run": true,
        "refused": { "by": by.as_str(), "verdict": verdict.as_str() },
    })
}

/// A tool-result outcome the runner produces for a call that did not run.
/// Registry misses, user rejections, run interruptions, and checks withheld
/// after an edit did not complete are recorded this way, so the model sees a
/// lossless tool_result for every call it emitted.
pub(crate) enum FallbackToolOutcome {
    /// A tool call whose name is not in the registry. on_resume distinguishes
    /// the resume-path miss (the tool was removed between run and resume) from
    /// the dispatch-path miss.
    UnknownTool { name: String, on_resume: bool },
    /// The user rejected the approval for this tool call.
    Rejected,
    /// The run was interrupted with a tool call pending a result.
    Interrupted,
    /// A shell check whose preceding file edit did not complete.
    SkippedAfterEdit,
}

impl FallbackToolOutcome {
    /// The model-visible tool_result payload for this outcome.
    ///
    /// Every outcome carries the not-run marker: the reason text explains why
    /// the call never executed, and the marker keeps a reader from counting
    /// that text as a verdict on an execution that did not happen. A user
    /// rejection also carries the refusal marker, because a refusal is a
    /// decision against the call rather than a call left unanswered.
    pub(crate) fn to_json(&self) -> Value {
        let error = match self {
            Self::UnknownTool {
                on_resume: true, ..
            } => "unknown tool on resume".to_string(),
            Self::UnknownTool {
                name,
                on_resume: false,
            } => format!("unknown tool: {name}"),
            Self::Rejected => {
                return refused_json(
                    "rejected by user".to_string(),
                    RefusalBy::User,
                    RefusalVerdict::Reject,
                );
            }
            Self::Interrupted => "interrupted by user".to_string(),
            Self::SkippedAfterEdit => SKIPPED_AFTER_EDIT.to_string(),
        };
        serde_json::json!({ "error": error, "not_run": true })
    }
}

#[cfg(test)]
mod tests;
