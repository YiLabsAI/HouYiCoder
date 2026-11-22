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

use crate::agent::RunOutcome;
use crate::agent::RunResult;
use crate::agent::{RunError, Runner};
use houyicoder_context::EventId;
use houyicoder_protocol::llm::Usage;
use tokio_util::sync::CancellationToken;

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

    pub(crate) fn set_turn_cancel(&self, token: Option<CancellationToken>) {
        if let Ok(mut guard) = self.turn_cancel.lock() {
            *guard = token;
        }
    }

    /// Cancel the active step without ending the run and queue one final
    /// synthesis directive as an internal MetaUser event.
    pub fn request_wrap_up(&self, directive: impl Into<String>) {
        if self
            .user_turn
            .lock()
            .expect("user_turn lock")
            .begin_wrap_up(directive.into(), true, false)
        {
            self.cancel_turn();
        }
    }

    /// Enter tool-disabled wrap-up after exhausting the subagent turn budget,
    /// granting one extra synthesis turn beyond max_turns.
    pub(crate) fn begin_turn_limit_wrap_up(&self) {
        const TURN_WRAP_DIRECTIVE: &str = "Your turn budget has ended and tool calls are now disabled. Immediately summarize your verified findings and remaining unverified items in plain text.";
        self.user_turn
            .lock()
            .expect("user_turn lock")
            .begin_wrap_up(TURN_WRAP_DIRECTIVE.to_string(), true, true);
    }

    /// Effective max_turns including at most one extra synthesis turn granted
    /// by a watchdog or subagent turn-limit wrap-up.
    pub(crate) fn effective_turn_limit(&self) -> u32 {
        let extra = u32::from(
            self.user_turn
                .lock()
                .expect("user_turn lock")
                .extra_wrap_turn(),
        );
        self.config.max_turns.saturating_add(extra)
    }

    /// Advance the turn counter and return the active turn limit if the turn
    /// may proceed, entering tool-disabled wrap-up when a subagent exhausts
    /// its configured turn budget.
    pub(crate) fn advance_turn_budget(&self) -> Option<u32> {
        let spent = self.bump_user_turn();
        if spent > self.config.max_turns
            && self.agent_identity.depth > 0
            && self.config.max_turns > 0
            && !self.is_wrapping_up()
        {
            self.begin_turn_limit_wrap_up();
        }
        let limit = self.effective_turn_limit();
        (spent <= limit).then_some(limit)
    }

    /// Whether a budget watchdog has asked this run to synthesize and exit.
    pub fn is_wrapping_up(&self) -> bool {
        self.user_turn
            .lock()
            .expect("user_turn lock")
            .is_wrapping_up()
    }

    /// Whether wrap-up was triggered by exhausting the subagent turn budget.
    pub fn turn_limit_wrapped(&self) -> bool {
        self.user_turn
            .lock()
            .expect("user_turn lock")
            .turn_limit_wrapped()
    }

    /// The durable event id of the MetaUser wrap-up directive, when written.
    pub fn wrap_up_event_id(&self) -> Option<EventId> {
        self.user_turn
            .lock()
            .expect("user_turn lock")
            .wrap_up_event_id()
    }

    /// Compute effective token spend weighted by the active cost model.
    pub fn effective_tokens(&self, usage: &Usage) -> u64 {
        let c = self.cost_model.cost();
        let cache_read = f64::from(usage.cache_read_input_tokens);
        let cache_write = f64::from(usage.cache_write_input_tokens);
        let non_cached = if usage.non_cached_input_tokens > 0
            || usage.cache_read_input_tokens > 0
            || usage.cache_write_input_tokens > 0
        {
            f64::from(usage.non_cached_input_tokens)
        } else {
            (self.cost_model.effective_input_tokens(usage) - cache_read - cache_write).max(0.0)
        };
        let weighted = non_cached * c.input
            + cache_read * c.cache_read
            + cache_write * c.cache_write_5m
            + f64::from(usage.output_tokens) * c.output;
        weighted.round().max(0.0) as u64
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

    /// The interrupted result for a cancel the drive loop observes while it is
    /// live: reconcile orphan tool results so every ToolCall keeps a
    /// ToolResult, then report Interrupted. Called where the loop is about to
    /// report a successful outcome — after tool dispatch, and before an
    /// answer — because the turn's own tail work does not read the token.
    pub(crate) async fn interrupted_at_boundary(
        &self,
        session: houyicoder_context::SessionId,
        turn: u32,
        usage: Usage,
    ) -> Result<RunResult, RunError> {
        self.reconcile_tool_results(session).await?;
        Ok(RunResult {
            outcome: RunOutcome::Interrupted("interrupted by user".to_string()),
            turns: turn,
            usage,
        })
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
    ) -> Result<Option<RunResult>, RunError> {
        // This resume consumes the pause: clear it so a later abort (while
        // drive_loop runs) does not set the durable flag (no paused run).
        self.paused.store(false, Ordering::Release);
        if !self.take_aborted() {
            return Ok(None);
        }
        self.reconcile_tool_results(session).await?;
        Ok(Some(RunResult {
            outcome: RunOutcome::Interrupted("interrupted by user".to_string()),
            turns: self.user_turn(),
            usage: Usage::default(),
        }))
    }
}

#[cfg(test)]
mod tests;
