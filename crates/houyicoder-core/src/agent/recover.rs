//! Crash-recovery re-drive: after a process crash mid-turn, the durable
//! session log holds the partial turn events. The recover method emits a
//! TurnAborted boundary marker so the partial content and the regenerated
//! content are not silently concatenated, then re-enters the drive loop. The
//! model sees the full history including the partial turn tool results, so
//! it regenerates the reply without re-requesting tools whose results are
//! already durable — the idempotency invariant: a ToolResult in the log is
//! never re-executed, only the model reply is regenerated.

use houyicoder_context::{SessionEvent, SessionId};
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
        let marked = self
            .store
            .append(new_event(
                session,
                SessionEvent::TurnAborted {
                    reason: "process restart".into(),
                },
            ))
            .await;
        if let Err(e) = marked {
            // The boundary marker is part of the re-driven turn: if it cannot
            // be written the turn ends here, and settling keeps the frontend
            // fold boundary even though the log is refusing writes.
            let failed: Result<RunResult, RunError> = Err(e.into());
            self.settle_turn(session, None, &failed).await;
            return failed;
        }
        // A recovery attempt is a fresh turn: reset the max_turns budget so a
        // session retried after a crash is not permanently capped.
        self.reset_user_turn();
        let started = std::time::Instant::now();
        let result = self.drive_loop(session, 0, Usage::default(), &token).await;
        // The re-drive is a drive loop like any other: the turn it ends needs
        // its record and its primary saves noticed, or the frontend has no
        // marker closing the regenerated turn and its summary row would fold
        // into the next turn's.
        self.settle_turn(session, Some(started), &result).await;
        result
    }
}
