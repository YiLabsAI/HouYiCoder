//! The abort surface: a durable flag that survives across the Interruption
//! boundary so a cancel issued while a run is PAUSED on an approval ask
//! actually surfaces at resume() entry. The cancel token minted at run()/
//! resume() entry is stale by the time an Interruption has returned, so the
//! token alone cannot reach a paused run; this flag does. The token still
//! serves the in-flight stream-loop abort (mid-run / mid-resume selects).
//!
//! Flag lifetime: set by abort(), cleared at run() entry, taken (swap) at
//! resume() entry. The two clear sites cover both ends of the Interruption
//! boundary — a fresh run clears a stale flag from a prior cancel so a later
//! resume is not falsely skipped, and resume's take clears it on read so the
//! short-circuit fires at most once per cancel. The flag never survives
//! across a run().

use std::sync::atomic::Ordering;

use houyicoder_context::{SessionId, TurnEventKind};

use crate::agent::{RunError, Runner};

impl Runner {
    /// Cancel the in-flight run, if any, AND — only when the run is paused on
    /// an Interruption (an approval ask in flight) — mark it aborted so a
    /// resume() that follows short-circuits to Interrupted instead of
    /// re-entering the drive loop. The token cancels the in-flight stream
    /// loop; the durable flag covers the paused-on-ask case only (when
    /// paused is false — idle, run in flight, or resume in flight — the flag
    /// is NOT set, so no abort call can poison a later resume).
    pub fn abort(&self) {
        tracing::debug!(paused = self.paused.load(Ordering::Acquire), "abort called");
        // The durable flag is only meaningful when the run is paused on an
        // ask (paused == true). An abort while idle / run-in-flight /
        // resume-in-flight has no paused run to short-circuit, so setting the
        // flag would only poison a later, unrelated resume. The token still
        // cancels an in-flight stream loop (the mid-run / mid-resume case).
        if self.paused.load(Ordering::Acquire) {
            self.aborted.store(true, Ordering::Release);
        }
        if let Some(t) = self.cancel.lock().expect("cancel mutex").as_ref() {
            t.cancel();
            tracing::debug!("abort cancelled the token");
        }
    }

    /// Cancel the active turn's model call without killing the run. The
    /// drive loop observes the per-turn token in the model-call select; on
    /// abort it appends an interrupt marker + starts the next turn with a
    /// fresh token. No-op when no turn is in flight (the token is cleared
    /// between turns + after the run). Distinct from abort (terminal).
    pub fn cancel_turn(&self) {
        if let Ok(guard) = self.turn_cancel.lock()
            && let Some(t) = guard.as_ref()
        {
            t.cancel();
        }
    }

    /// Whether abort() was called since the last run() entry AND the run was
    /// paused (so the durable flag was set). The serve loop checks this after
    /// each handle_approval to break the approval batch.
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }

    /// Mark the run paused on an Interruption (an approval ask is in flight).
    /// Called at every RunOutcome::Interruption return so a subsequent abort()
    /// sets the durable flag. Cleared at run()/resume() entry (the run is no
    /// longer paused — it is entering drive_loop) and by aborted_short_circuit
    /// (the resume consumed the pause).
    pub(crate) fn mark_paused(&self) {
        self.paused.store(true, Ordering::Release);
    }

    /// Clear both the aborted flag and the paused flag. Called at run() entry
    /// so a fresh run is not poisoned by a stale flag from a prior cancel.
    pub(crate) fn reset_run_state(&self) {
        self.aborted.store(false, Ordering::Release);
        self.paused.store(false, Ordering::Release);
    }

    /// Take the durable abort flag (swap to false). resume() calls this via
    /// aborted_short_circuit at entry: if set, the run was cancelled during
    /// the ask-wait, so it short-circuits to Interrupted (after reconciling
    /// orphan results to keep the session lossless) instead of re-entering
    /// drive_loop.
    pub(crate) fn take_aborted(&self) -> bool {
        self.aborted.swap(false, Ordering::AcqRel)
    }

    /// The resume() entry short-circuit: clear the paused flag (this resume is
    /// consuming the pause — either it short-circuits on abort or enters
    /// drive_loop, neither paused), then if the durable abort flag is set (a
    /// cancel landed during the ask-wait, while the run was paused on an
    /// Interruption) reconcile orphan tool results so the session stays
    /// lossless (every ToolCall has a ToolResult — else the next message
    /// ships tool_calls without results and providers 400) and return
    /// Interrupted. None when the run was not aborted (resume proceeds
    /// normally). Matches the drive loop's in-flight abort path.
    pub(crate) async fn aborted_short_circuit(
        &self,
        session: houyicoder_context::SessionId,
    ) -> Result<Option<crate::agent::RunResult>, RunError> {
        // This resume consumes the pause: clear it so a later abort (while
        // drive_loop runs) does not set the durable flag (no paused run).
        self.paused.store(false, Ordering::Release);
        if !self.take_aborted() {
            return Ok(None);
        }
        self.reconcile_tool_results(session).await?;
        Ok(Some(crate::agent::RunResult {
            outcome: crate::agent::RunOutcome::Interrupted("interrupted by user".to_string()),
            turns: self.count_turns(session).await?,
            usage: houyicoder_protocol::llm::Usage::default(),
        }))
    }

    /// Count completed model calls by counting AssistantMessage events (each
    /// model call appends one) so an interrupted run reports how many turns
    /// completed before the abort (resume carries the max_turns cap over).
    pub(super) async fn count_turns(&self, session: SessionId) -> Result<u32, RunError> {
        let events = self.store.replay(session).await?;
        let n = events
            .iter()
            .filter(|e| matches!(e.kind, TurnEventKind::AssistantMessage { .. }))
            .count() as u32;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::RunOutcome;
    use crate::agent::ToolRegistry;
    use crate::agent::tests::runner_with;
    use houyicoder_context::SessionId;
    use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
    use std::sync::Arc;

    fn stub_runner() -> Runner {
        runner_with(
            Arc::new(crate::provider::test_support::FakeProvider::new(vec![
                CompletionResponse {
                    output: vec![OutputItem::Text {
                        text: "test".into(),
                    }],
                    usage: Usage::default(),
                    model: "test".into(),
                },
            ])),
            ToolRegistry::new(),
        )
    }

    /// abort() sets the durable flag; resume() checks it via
    /// aborted_short_circuit and short-circuits to Interrupted (after
    /// reconciling orphan results) instead of re-entering drive_loop. This is
    /// the path that makes a cancel issued during the ask-wait actually
    /// surface — the cancel token alone cannot reach a paused run. abort() only
    /// sets the durable flag when the run is paused (mark_paused), so an idle
    /// or in-flight abort cannot poison a later resume.
    #[tokio::test]
    async fn test_resume_short_circuits_abort() {
        let runner = stub_runner();
        let session = SessionId::new();
        // Simulate a run paused on an Interruption (the state mark_paused is
        // normally set at the Interruption return inside run/resume).
        runner.mark_paused();
        runner.abort();
        assert!(runner.is_aborted(), "abort sets the flag when paused");
        let result = runner.resume(session, &[]).await.expect("resume");
        assert!(
            matches!(result.outcome, RunOutcome::Interrupted(_)),
            "an aborted resume short-circuits to Interrupted, got {:?}",
            result.outcome
        );
        assert!(!runner.is_aborted(), "take_aborted cleared the flag");
    }

    /// An abort while NOT paused (idle / run in flight / resume in flight) does
    /// NOT set the durable flag — the token handles the in-flight case, and
    /// setting the flag would only poison a later, unrelated resume.
    #[tokio::test]
    async fn test_not_paused_skips_flag() {
        let runner = stub_runner();
        // No mark_paused — simulate an idle or in-flight cancel.
        runner.abort();
        assert!(
            !runner.is_aborted(),
            "abort while not paused must not set the durable flag"
        );
    }

    /// A fresh run() clears a stale aborted flag + the paused flag from a
    /// prior cancel so the next resume() on a later Interruption is not
    /// falsely skipped.
    #[tokio::test]
    async fn test_run_clears_stale_flag() {
        let runner = stub_runner();
        runner.mark_paused();
        runner.abort();
        assert!(runner.is_aborted());
        drop(runner.run(SessionId::new(), "go".into()).await);
        assert!(!runner.is_aborted(), "run() clears a stale abort flag");
    }
}
