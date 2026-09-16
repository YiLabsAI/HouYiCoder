use super::frame_log_msg;
use crate::agent_message::AgentMessage;
use crate::state::Pane;
use crate::test_harness::{connected_app_events, wait_for_request};
use crate::transcript::TranscriptFrame;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::session_update::{
    SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};

#[path = "tests/frames.rs"]
mod frames;
#[path = "tests/model.rs"]
mod model;
#[path = "tests/panels.rs"]
mod panels;
#[path = "tests/subagent_fold.rs"]
mod subagent_fold;
#[path = "tests/subagent_render.rs"]
mod subagent_render;
#[path = "tests/teammate_esc.rs"]
mod teammate_esc;
#[path = "tests/teammate_view.rs"]
mod teammate_view;

fn tool_call_frame(id: &str, title: &str, status: ToolCallStatus) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, title).status(status),
    ))
}

fn tool_done_frame(id: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id,
        ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
    )))
}

fn done_msg() -> AgentMessage {
    AgentMessage::Done {
        result: Ok(houyicoder_protocol::frontend::run::RunResult {
            outcome: houyicoder_protocol::frontend::run::RunOutcome::FinalOutput {
                content: vec![houyicoder_protocol::frontend::run::ContentBlock::Text {
                    text: "ok".into(),
                }],
            },
            usage: houyicoder_protocol::llm::Usage::default(),
            turns: 1,
            stop_reason: houyicoder_protocol::frontend::run::StopReason::EndTurn,
        }),
    }
}

fn todo_frame(items: &[(&str, &str)]) -> TranscriptFrame {
    let todos: Vec<serde_json::Value> = items
        .iter()
        .map(|(content, status)| serde_json::json!({"content": content, "status": status}))
        .collect();
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new("todo_1", "todo_write").raw_input(serde_json::json!({"todos": todos})),
    ))
}

/// A memory-change broadcast while the /memory pane is open refreshes the list
/// over the wire so the pane reflects the new entries.
#[test]
fn test_memory_change_refreshes_pane() {
    use houyicoder_protocol::frontend::memory::{
        MemoryChange, MemoryChangeId, MemoryChangeOrigin, MemoryOperation,
    };
    let (mut app, events) = connected_app_events();
    app.pane = Pane::Memory;
    app.handle_agent_message(AgentMessage::MemoryChanged {
        id: MemoryChangeId("change-1".into()),
        origin: MemoryChangeOrigin::AutoMemory,
        changes: vec![MemoryChange {
            key: "alpha".into(),
            operation: MemoryOperation::Stored,
        }],
    });
    let req = wait_for_request(&events, |p| matches!(p, FrontendRequest::MemoryList));
    assert_eq!(req.req_id.0, 0, "first request on a fresh session");
}
