//! Tests for transcript folding.

use std::sync::Arc;

use super::TranscriptFold;
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields,
};
use houyicoder_session::SessionStore;
use serde_json::json;

fn update_notification(update: &SessionUpdate) -> AcpNotification {
    AcpNotification::new(
        "session/update",
        json!({ "update": serde_json::to_value(update).unwrap() }),
    )
}

fn fold_with_log() -> (TranscriptFold, Arc<SessionStore>, SessionId) {
    let session = SessionId::new();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let fold = TranscriptFold::new(session, store.clone());
    (fold, store, session)
}

/// Agent message chunks accumulate and flush as one AssistantMessage when
/// flush is called.
#[tokio::test]
async fn test_chunks_flush_as_message() {
    let (mut fold, store, session) = fold_with_log();
    fold.handle_notification(&update_notification(&SessionUpdate::AgentMessageChunk(
        ContentChunk::new(ContentBlock::Text { text: "hel".into() }),
    )))
    .await
    .unwrap();
    fold.handle_notification(&update_notification(&SessionUpdate::AgentMessageChunk(
        ContentChunk::new(ContentBlock::Text { text: "lo".into() }),
    )))
    .await
    .unwrap();
    let entries = store.replay(session).await.unwrap();
    assert!(entries.is_empty(), "text accumulates, not appended yet");
    fold.flush().await.unwrap();
    let entries = store.replay(session).await.unwrap();
    assert_eq!(entries.len(), 1, "one AssistantMessage");
    assert!(matches!(
        &entries[0].event,
        SessionEvent::AssistantMessage { text, .. } if text == "hello"
    ));
}

/// A tool call flushes pending assistant text then appends the call.
#[tokio::test]
async fn test_tool_call_flushes_text() {
    let (mut fold, store, session) = fold_with_log();
    fold.handle_notification(&update_notification(&SessionUpdate::AgentMessageChunk(
        ContentChunk::new(ContentBlock::Text {
            text: "see the file".into(),
        }),
    )))
    .await
    .unwrap();
    fold.handle_notification(&update_notification(&SessionUpdate::ToolCall(
        ToolCall::new("call-1", "bash"),
    )))
    .await
    .unwrap();
    let entries = store.replay(session).await.unwrap();
    assert_eq!(entries.len(), 2, "assistant message then tool call");
    assert!(matches!(
        &entries[0].event,
        SessionEvent::AssistantMessage { text, .. } if text == "see the file"
    ));
    assert!(matches!(
        &entries[1].event,
        SessionEvent::ToolCall { call_id, .. } if call_id == "call-1"
    ));
}

/// A completed tool update appends a ToolResult.
#[tokio::test]
async fn test_completed_appends_result() {
    let (mut fold, store, session) = fold_with_log();
    let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::new("call-1"),
        ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
    ));
    fold.handle_notification(&update_notification(&update))
        .await
        .unwrap();
    let entries = store.replay(session).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert!(matches!(
        &entries[0].event,
        SessionEvent::ToolResult { call_id, .. } if call_id == "call-1"
    ));
}

/// An in-progress tool update does not append anything.
#[tokio::test]
async fn test_in_progress_skips_result() {
    let (mut fold, store, session) = fold_with_log();
    let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::new("call-1"),
        ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
    ));
    fold.handle_notification(&update_notification(&update))
        .await
        .unwrap();
    let entries = store.replay(session).await.unwrap();
    assert!(entries.is_empty(), "in-progress does not append");
}

/// A non-session/update notification is ignored.
#[tokio::test]
async fn test_ignores_other_methods() {
    let (mut fold, store, session) = fold_with_log();
    let notif = AcpNotification::new("session/cancel", json!({}));
    fold.handle_notification(&notif).await.unwrap();
    let entries = store.replay(session).await.unwrap();
    assert!(entries.is_empty());
}

/// Reasoning chunks accumulate and flush alongside the assistant message.
#[tokio::test]
async fn test_reasoning_flushed_with_message() {
    let (mut fold, store, session) = fold_with_log();
    fold.handle_notification(&update_notification(&SessionUpdate::AgentThoughtChunk(
        ContentChunk::new(ContentBlock::Text {
            text: "thinking".into(),
        }),
    )))
    .await
    .unwrap();
    fold.handle_notification(&update_notification(&SessionUpdate::AgentMessageChunk(
        ContentChunk::new(ContentBlock::Text {
            text: "answer".into(),
        }),
    )))
    .await
    .unwrap();
    fold.flush().await.unwrap();
    let entries = store.replay(session).await.unwrap();
    assert_eq!(entries.len(), 1);
    match &entries[0].event {
        SessionEvent::AssistantMessage { text, thinking } => {
            assert_eq!(text, "answer");
            assert_eq!(thinking.as_deref(), Some("thinking"));
        }
        other => panic!("expected AssistantMessage, got {other:?}"),
    }
}
