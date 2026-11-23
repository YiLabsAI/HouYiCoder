//! Transcript folding: converts ACP session/update notifications into durable
//! SessionLog entries so the child's conversation is recorded in the parent's
//! session log alongside in-process children.

use std::mem::take;
use std::sync::Arc;

use houyicoder_api::session::SessionLog;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{SessionUpdate, ToolCallStatus};
use serde_json::Value;

/// Folds session/update notifications from an external ACP child into the
/// parent's session log, so the child's transcript is durable and replayable.
pub struct TranscriptFold {
    session: SessionId,
    log: Arc<dyn SessionLog>,
    assistant_text: String,
    reasoning_text: String,
}

impl TranscriptFold {
    /// Build a fold sink for one prompt round.
    pub fn new(session: SessionId, log: Arc<dyn SessionLog>) -> Self {
        Self {
            session,
            log,
            assistant_text: String::new(),
            reasoning_text: String::new(),
        }
    }

    /// Inspect one notification and fold its update into the log if it
    /// carries a standard session/update payload. Non-update notifications
    /// are ignored.
    pub async fn handle_notification(&mut self, notif: &AcpNotification) -> Result<(), String> {
        if notif.method != "session/update" {
            return Ok(());
        }
        let Some(params) = &notif.params else {
            return Ok(());
        };
        let update: SessionUpdate =
            serde_json::from_value(params.get("update").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("session/update malformed: {e}"))?;
        self.fold_update(&update).await
    }

    async fn fold_update(&mut self, update: &SessionUpdate) -> Result<(), String> {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let ContentBlock::Text { text } = &chunk.content {
                    self.assistant_text.push_str(text);
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                if let ContentBlock::Text { text } = &chunk.content {
                    self.reasoning_text.push_str(text);
                }
            }
            SessionUpdate::ToolCall(call) => {
                self.flush_assistant().await?;
                self.append(SessionEvent::ToolCall {
                    call_id: call.tool_call_id.0.clone(),
                    tool: call.title.clone(),
                    input: call.raw_input.clone().unwrap_or(Value::Null),
                })
                .await?;
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let done = matches!(
                    update.fields.status,
                    Some(ToolCallStatus::Completed) | Some(ToolCallStatus::Failed)
                );
                if done {
                    self.append(SessionEvent::ToolResult {
                        call_id: update.tool_call_id.0.clone(),
                        output: update.fields.raw_output.clone().unwrap_or(Value::Null),
                        duration_ms: update.fields.duration_ms.unwrap_or(0),
                    })
                    .await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn flush_assistant(&mut self) -> Result<(), String> {
        if self.assistant_text.is_empty() {
            return Ok(());
        }
        let text = take(&mut self.assistant_text);
        let thinking = if self.reasoning_text.is_empty() {
            None
        } else {
            Some(take(&mut self.reasoning_text))
        };
        self.append(SessionEvent::AssistantMessage { text, thinking })
            .await
    }

    /// Flush remaining text. Call when the prompt round ends.
    pub async fn flush(&mut self) -> Result<(), String> {
        self.flush_assistant().await
    }

    async fn append(&self, event: SessionEvent) -> Result<(), String> {
        let entry = SessionLogEntry {
            id: EventId::new(),
            session: self.session,
            ts: 0,
            prev_hash: None,
            event,
        };
        self.log
            .append(entry)
            .await
            .map(|_| ())
            .map_err(|e| format!("log append failed: {e}"))
    }
}

#[cfg(test)]
mod tests;
