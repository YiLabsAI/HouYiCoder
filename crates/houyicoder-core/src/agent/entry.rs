//! Public run entries: run, run_forked, resume. Each is a thin wrapper over
//! drive_loop that sets up per-run state (cancel token, fact extraction,
//! memory recall) at the boundaries a caller hits. Extracted from the main
//! impl so the entry surface and the loop body live apart.

use houyicoder_context::{SessionId, TurnEvent};
use houyicoder_protocol::llm::Usage;
use tokio_util::sync::CancellationToken;

use super::append::new_event;
use super::fact;
use super::{ApprovalDecision, RunError, RunOutcome, RunResult, Runner};

impl Runner {
    /// Run the agent on a user input. Appends the user event, then drives the
    /// loop: RunAgain → prepare + complete + append + resolve; FinalOutput →
    /// return; Handoff → return; Interruption → return (caller resumes).
    pub async fn run(&self, session: SessionId, user_input: String) -> Result<RunResult, RunError> {
        // Prune expired snapshots at the start of each run — a natural trigger
        // (a new run is starting, clean up old snapshots that exceed TTL/size cap).
        // Skips snapshots still referenced by the undo stack.
        self.prune_snapshots();
        self.reset_run_state();
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        // Deterministic fact extraction: scan the user input for explicit
        // save signals before appending. Extracted facts are written
        // atomically after the run completes so the store reflects them
        // for the next session. No model classifier on the hot path — only
        // structured patterns the user types deliberately.
        let pending_facts = fact::extract_save_facts(&user_input);
        // Reconcile orphan ToolCall from a prior hard crash / disconnect before
        // appending this turn's user input. The interrupted result must land
        // adjacent to its tool_use: build_request_body emits role:"tool", which
        // must immediately follow the assistant turn that issued the call.
        // Appending user input first would interpose role:"user" and still 400.
        // resume() does not reconcile — pending approvals are re-raised, not voided.
        self.reconcile_tool_results(session).await?;
        self.append_user_input(session, user_input).await?;
        // Turn-entry memory recall: scan the projected transcript for the
        // surfaced de-dup set, recall entries relevant to this turn's query,
        // and append a durable memory-recall attachment the projection merges
        // into this turn's user message. The system prompt stays byte-frozen
        // (memory is in the message stream, not the prompt) so prompt-cache
        // survives across turns. No-op when no memory provider is wired.
        self.inject_memory_recall(session).await?;
        let result = self
            .drive_loop(session, 0, Usage::default(), &token)
            .await?;
        // Persist extracted facts after the run. Failures are logged but
        // never fail the run — memory persistence is best-effort, not a
        // hard gate on the agent loop.
        if let Some(memory) = &self.memory {
            for entry in pending_facts {
                if let Err(e) = memory.add(entry) {
                    tracing::warn!("memory write failed: {e}");
                }
            }
        }
        Ok(result)
    }

    /// Run on a session pre-seeded with a cloned event prefix (re-stamped
    /// to the forked session id) plus a user input. Used by the forked
    /// extraction runner: the main conversation is replayed into a fresh
    /// ephemeral session, the extraction prompt is the user input, drive_loop
    /// runs with the forked config. No fact extraction (the forked agent
    /// emits structured save-memory tool calls). The caller guarantees the
    /// prefix is consistent (forking at a stop boundary -- final response,
    /// no tool calls -- ensures no orphan ToolCall without a ToolResult).
    pub async fn run_forked(
        &self,
        session: SessionId,
        prefix: &[TurnEvent],
        user_input: String,
    ) -> Result<RunResult, RunError> {
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        for ev in prefix {
            self.store
                .append(new_event(session, ev.kind.clone()))
                .await?;
        }
        self.append_user_input(session, user_input).await?;
        self.drive_loop(session, 0, Usage::default(), &token).await
    }

    /// Continue a run paused on NextStep::Interruption. For each approval
    /// request the caller passes a decision for, the decision is applied:
    /// approved ⇒ execute the tool and append its result; rejected ⇒ append a
    /// rejection-note result. Pending approvals WITHOUT a matching decision are
    /// LEFT pending (no ToolResult appended) — the caller raises them one at a
    /// time. If any remain undecided, resume returns a fresh Interruption
    /// carrying the remainder so the caller shows the next approval dialog.
    /// Only when all have a decision does the loop resume (RunAgain). The
    /// ToolCall events are already in the log; resume only adds the matching
    /// ToolResults — no counter rewind (lossless log). The turn counter
    /// continues from the prior run (from the log) so max_turns is cumulative
    /// across run + resume; usage restarts at zero (not persisted).
    pub async fn resume(
        &self,
        session: SessionId,
        decisions: &[ApprovalDecision],
    ) -> Result<RunResult, RunError> {
        if let Some(r) = self.aborted_short_circuit(session).await? {
            // Abort path: Interrupted is terminal, but this skips drive_loop
            // (so the loop-exit finalize does not run). Finalize here.
            let result = Ok(r);
            self.finalize_input_buffer(&result);
            return result;
        }
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        let remaining = self.apply_decisions(session, decisions).await?;
        if !remaining.is_empty() {
            // Partial decision set: re-interrupt for the undecided calls so the
            // caller raises the next approval dialog. The decided calls already
            // have their ToolResults appended; only the undecided calls appear
            // here. turns is reported from the log so the cap stays cumulative.
            let prior_turns = self.count_turns(session).await?;
            self.mark_paused();
            return Ok(RunResult {
                outcome: RunOutcome::Interruption(remaining),
                turns: prior_turns,
                usage: Usage::default(),
            });
        }
        let prior_turns = self.count_turns(session).await?;
        self.drive_loop(session, prior_turns, Usage::default(), &token)
            .await
    }
}
