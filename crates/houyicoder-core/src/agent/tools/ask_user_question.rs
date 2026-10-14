//! Human-in-the-loop clarification tool. The model invokes this when intent is
//! ambiguous or a decision genuinely belongs to the user: it emits a tool call
//! carrying one to four multiple-choice questions, and the loop pauses for a
//! human to answer before the tool result lands.
//!
//! Placement: a registered tool the model triggers, reusing the
//! approval-and-resume pause mechanism (requires_approval returns true, so
//! resolve_turn collects the call into an Interruption and returns; the host
//! renders a card, the user answers, resume injects the answers into the input
//! via an ApprovalDecision and runs execute_authorized). The tool itself does
//! no work beyond formatting the answer — it only suspends the run, like a
//! permission prompt.
//!
//! Read-only (no workspace side effect), approval-required (always pauses).
//! Concurrency-safe because pausing holds no resource.

use houyicoder_async::PFut;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};

/// A multiple-choice clarification tool. The model calls it with questions;
/// the host UI collects answers and resume() runs execute_authorized with the
/// answer-populated input. Statelesss: the answers live in the input the UI
/// assembles, not in the tool.
pub struct AskUserQuestionTool;

impl AskUserQuestionTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AskUserQuestionTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "AskUserQuestion"
    }
    fn description(&self) -> &str {
        "Ask the user multiple-choice questions to gather information, clarify \
         ambiguity, understand preferences, make decisions, or offer choices. \
         Use only when blocked on a decision that is genuinely the user's to \
         make; do not use when you can verify the answer, have a sensible \
         default, or can decide yourself. Pass 1-4 questions, each with a 2-12 \
         character header and 2-4 options (label + description). The user may \
         pick 'Other' to type a custom answer. Set multiSelect true to allow \
         multiple selections. The tool pauses the run until the user answers."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "question": {"type": "string"},
                            "header": {"type": "string", "maxLength": 12},
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string"},
                                        "preview": {"type": "string"}
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multiSelect": {"type": "boolean", "default": false}
                        },
                        "required": ["question", "header", "options", "multiSelect"]
                    }
                }
            },
            "required": ["questions"]
        })
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        // Fail-closed: the model must not run this inline. The loop collects
        // approval-requiring tools into an Interruption and the host UI
        // collects answers; resume calls execute_authorized with the
        // answer-populated input. Reaching execute means the run bypassed the
        // approval path, which is a misuse.
        Box::pin(async {
            Err(ToolError::Failed(
                "AskUserQuestion requires human input; it must run via the approval path".into(),
            ))
        })
    }
    fn execute_authorized(
        &self,
        _ctx: ToolCtx,
        input: Value,
    ) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move { Ok(format_answer_result(&input)) })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

/// The reject directive the model sees when the user vetos a tool use plain
/// (no reason). The TUI shows a short "User declined" label; the model sees this full directive so
/// it stops and waits for the user rather than retrying.
const REJECT_MESSAGE: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";

/// Format the tool result the model sees: the questions plus the user's
/// answers as a readable summary, plus the structured answers map. Answers
/// arrive in the answers object the UI injected (question text to selected
/// label, comma-joined labels for multiSelect, or custom text for Other).
/// Returns None-style empty answers when the UI injected none, so the model
/// still gets a well-formed result.
fn format_answer_result(input: &Value) -> Value {
    // Cancel path: the UI sends a declined marker rather than a plain reject
    // so this formatter controls the model-visible text. The model sees the
    // full reject directive (stop and wait); the TUI separately renders a
    // short "User declined" label from the declined flag (see brief.rs), so the
    // human-facing label and the model-facing directive stay decoupled.
    if input
        .get("declined")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return json!({
            "declined": true,
            "summary": REJECT_MESSAGE,
        });
    }
    let questions = input.get("questions").and_then(|v| v.as_array());
    let answers = input.get("answers").cloned().unwrap_or(json!({}));
    let mut parts: Vec<String> = Vec::new();
    if let Some(qs) = questions {
        for q in qs {
            let question = q.get("question").and_then(|v| v.as_str()).unwrap_or("");
            let answer = answers.get(question).and_then(|v| v.as_str()).unwrap_or("");
            // Quote both sides so a question or answer with commas does
            // not confuse the comma-joined shape.
            parts.push(format!("{question:?}={answer:?}"));
        }
    }
    let summary = format!(
        "User has answered your questions: {}. You can now continue with the user's answers in mind.",
        parts.join(", ")
    );
    json!({
        "answered": true,
        "answers": answers,
        "summary": summary,
    })
}

#[cfg(test)]
mod tests;
