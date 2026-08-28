//! Step layer: NextStep, TurnOutcome, and the human-in-the-loop approval
//! types.
//!
//! NextStep is a four-variant discriminated union with typed payloads
//! (no any): RunAgain, FinalOutput, Handoff, Interruption. The decision
//! ladder (resolve_turn) order: handoffs short-circuit; pending tools ⇒
//! run_again; otherwise the last text message is the final output. A turn
//! with pending tools is never final.
//!
//! Approval (HITL): a tool that declares requires_approval is NOT executed on
//! first pass — the loop collects all approval-requiring calls in the turn
//! into a NextStep::Interruption and returns. The caller decides
//! approve/reject per call, then resume() appends the approved results (and
//! rejection notes for the rejected) and continues. The engine's lossless log
//! makes resume tractable: the ToolCall events are already appended; resume
//! only adds the matching ToolResults — no counter rewind (the lossless log
//! makes a persisted-item counter unnecessary).

use houyicoder_context::AgentId;
use houyicoder_protocol::llm::OutputItem;

/// What the loop does next, computed once per turn by resolve_turn. A
/// NextStep union with typed payloads.
#[derive(Debug)]
pub enum NextStep {
    /// Call the model again (tools were called and executed; feed results back
    /// by re-projecting the event log next iteration).
    RunAgain,
    /// The turn produced a final answer. The loop returns.
    FinalOutput(String),
    /// The turn handed off to another agent. terminal — the run returns
    /// and the caller (a future orchestrator) spawns the target. Full
    /// swap-and-continue lands with the multi-agent runtime.
    Handoff(AgentId),
    /// One or more tools require human approval before executing. The loop
    /// returns; the caller approves/rejects, then resume() continues.
    Interruption(Vec<ApprovalRequest>),
}

/// A request to approve a tool call before it executes. The referenced
/// ToolCall event is already in the session log (by call_id); approval merely
/// decides whether resume() executes it.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// The tool call id as it appears in TurnEventKind::ToolCall.call_id and
    /// OutputItem::ToolCall.id.
    pub call_id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
}

impl ApprovalRequest {
    pub fn new(call_id: String, tool_name: String, input: serde_json::Value) -> Self {
        Self {
            call_id,
            tool_name,
            input,
        }
    }
}

/// A caller's decision on one approval request. Rejected calls get a
/// {"error": "rejected by user"} tool result so the model sees the veto.
/// updated_input carries an answer-populated input the human-in-the-loop UI
/// injected (e.g. AskUserQuestion answers); when Some, resume() executes the
/// tool with this input instead of the original call input so the collected
/// answers reach the tool. None for a plain approve/reject.
#[derive(Debug, Clone)]
pub struct ApprovalDecision {
    pub call_id: String,
    pub approved: bool,
    pub updated_input: Option<serde_json::Value>,
}

impl ApprovalDecision {
    pub fn approve(call_id: &str) -> Self {
        Self {
            call_id: call_id.to_string(),
            approved: true,
            updated_input: None,
        }
    }
    pub fn reject(call_id: &str) -> Self {
        Self {
            call_id: call_id.to_string(),
            approved: false,
            updated_input: None,
        }
    }
    /// Approve a call with an updated input the UI assembled (e.g.
    /// AskUserQuestion answers merged into the original questions). resume()
    /// runs execute_authorized with this input instead of the model's original.
    pub fn approve_with_input(call_id: &str, input: serde_json::Value) -> Self {
        Self {
            call_id: call_id.to_string(),
            approved: true,
            updated_input: Some(input),
        }
    }
}

/// The resolved outcome of one turn: the next step plus the usage to attribute.
/// Computed once (no multi-flag post-hoc branching).
#[derive(Debug)]
pub struct TurnOutcome {
    pub next_step: NextStep,
}

/// Extract the final-text answer from a model response (the last Text
/// output). None when the model emitted no text — the loop treats that as
/// run_again (model said nothing usable).
pub fn extract_final_text(output: &[OutputItem]) -> Option<String> {
    output.iter().rev().find_map(|o| match o {
        OutputItem::Text { text } => Some(text.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_protocol::llm::OutputItem;

    #[test]
    fn test_extract_final_text_takes() {
        let out = vec![
            OutputItem::Text {
                text: "first".into(),
            },
            OutputItem::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({}),
            },
            OutputItem::Text {
                text: "final answer".into(),
            },
        ];
        assert_eq!(extract_final_text(&out).as_deref(), Some("final answer"));
    }

    #[test]
    fn test_extract_final_text_none() {
        let out = vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            input: serde_json::json!({}),
        }];
        assert!(extract_final_text(&out).is_none());
    }

    #[test]
    fn test_approval_decision_constructors() {
        assert!(ApprovalDecision::approve("c1").approved);
        assert!(!ApprovalDecision::reject("c1").approved);
        // Plain approve/reject carry no updated input.
        assert!(ApprovalDecision::approve("c1").updated_input.is_none());
        assert!(ApprovalDecision::reject("c1").updated_input.is_none());
    }

    #[test]
    fn test_decision_carries_updated_input() {
        // AskUserQuestion answers ride on the decision so resume() runs the
        // tool with the answer-populated input, not the model's original.
        let d = ApprovalDecision::approve_with_input(
            "c1",
            serde_json::json!({"questions": [], "answers": {"q": "a"}}),
        );
        assert!(d.approved);
        let input = d.updated_input.expect("updated input present");
        assert_eq!(input["answers"]["q"], "a");
    }
}
