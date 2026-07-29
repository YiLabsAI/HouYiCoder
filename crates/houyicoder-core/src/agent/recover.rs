//! Crash-recovery re-drive: after a process crash mid-turn, the durable
//! session log holds the partial turn events. The recover method emits a
//! TurnAborted boundary marker so the partial content and the regenerated
//! content are not silently concatenated, then re-enters the drive loop. The
//! model sees the full history including the partial turn tool results, so
//! it regenerates the reply without re-requesting tools whose results are
//! already durable — the idempotency invariant: a ToolResult in the log is
//! never re-executed, only the model reply is regenerated.

use houyicoder_context::{SessionId, TurnEventKind};
use houyicoder_protocol::llm::Usage;
use tokio_util::sync::CancellationToken;

use super::append::new_event;
use super::{RunError, RunResult, Runner};

impl Runner {
    /// Re-drive a turn interrupted by a process crash. The durable session
    /// log already holds the partial turn events (user input, tool calls,
    /// tool results). This emits a TurnAborted boundary marker so the partial
    /// content and the regenerated content are not silently concatenated,
    /// then re-enters the drive loop. The model sees the full history
    /// including the partial turn tool results, so it regenerates the reply
    /// without re-requesting tools whose results are already durable.
    pub async fn recover_turn(&self, session: SessionId) -> Result<RunResult, RunError> {
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        self.store
            .append(new_event(
                session,
                TurnEventKind::TurnAborted {
                    reason: "process restart".into(),
                },
            ))
            .await?;
        self.drive_loop(session, 0, Usage::default(), &token).await
    }
}
