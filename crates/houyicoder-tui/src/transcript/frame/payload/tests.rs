use super::*;
use crate::transcript::records::{SubagentFinished, SubagentOutcome};
use houyicoder_protocol::frontend::session_update::{
    Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, SessionUpdate, ToolCallUpdate,
    ToolCallUpdateFields,
};

fn sized(frame: TranscriptFrame) -> usize {
    SequencedFrame::from(frame).estimated_bytes()
}

/// The two payloads a frame can carry without bound, an image's encoded
/// data and a tool's output, must reach the resident total; a budget that
/// counts them as nothing never evicts the frames that exhaust memory.
#[test]
fn test_sizes_image_and_output() {
    let image = TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Image {
            data: "A".repeat(5000),
            mime_type: "image/png".into(),
        },
    )));
    let result = TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "call-1",
        ToolCallUpdateFields::new().raw_output(serde_json::json!({ "stdout": "B".repeat(5000) })),
    )));
    let text = TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Text {
            text: "C".repeat(20),
        },
    )));

    assert!(sized(image) >= 5000, "an image's data is resident");
    assert!(sized(result) >= 5000, "a tool result's output is resident");
    assert!(sized(text) < 500, "a short text frame rides its overhead");
}

/// A frame whose payload is a plan counts the step text it holds. Sizing only
/// the streaming frames leaves every other resident payload carried by a frame
/// that reports no cost, and a window of those never reaches its budget.
#[test]
fn test_sizes_plan_entries() {
    let step = "read the session log and total the bytes it holds".repeat(2);
    let plan = TranscriptFrame::Session(SessionUpdate::Plan(Plan {
        entries: vec![PlanEntry {
            content: step.clone(),
            priority: PlanEntryPriority::High,
            status: PlanEntryStatus::Pending,
            meta: None,
        }],
        meta: None,
    }));

    assert!(
        sized(plan) >= FRAME_OVERHEAD + step.len(),
        "a plan's step text is resident"
    );
}

/// A trace the frontend raised carries the task text and the child's id, and
/// both are resident for as long as the row is. Counting it as overhead alone
/// would let a window of long task descriptions sit past its budget.
#[test]
fn test_sizes_trace_text() {
    let task = "map every route the auth middleware guards".repeat(4);
    let trace = TranscriptFrame::Frontend(FrontendRow::SubagentFinished(SubagentFinished {
        child_sid: "child-xyz".into(),
        subagent_type: "explore".into(),
        description: task.clone(),
        outcome: SubagentOutcome::Finished,
        duration_ms: Some(12_000),
        tool_uses: 7,
    }));

    assert!(
        sized(trace) >= FRAME_OVERHEAD + task.len(),
        "the trace's task text is resident"
    );
}
