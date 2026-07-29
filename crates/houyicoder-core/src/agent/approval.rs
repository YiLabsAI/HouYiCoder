//! Approval flow for Runner: resolve a caller's decisions onto the pending
//! tool calls, reconcile orphan calls on abort, and enumerate which calls
//! still await a decision. Extracted from the main impl so the approval
//! path and its call_id-uniqueness invariant live together.

use houyicoder_api::tool::ToolCtx;
use houyicoder_context::{SessionId, TurnEventKind};

use super::synthetic::{SyntheticToolOutcome, tool_error_json};
use super::{ApprovalDecision, ApprovalRequest, RunError, Runner};

impl Runner {
    /// Apply a caller's approval decisions to the pending tool calls. Approved
    /// calls execute; rejected calls get a rejection-note result. Unknown tools
    /// (registry miss on resume) get an error result. Pending approvals WITHOUT
    /// a matching decision are LEFT pending — no ToolResult is appended for
    /// them, so they stay resolvable on a later resume() call. This enables
    /// one-at-a-time approval (the caller passes a single decision per resume,
    /// gets re-interrupted for the rest). Returns the still-pending approval
    /// requests (those with no matching decision) so the caller can re-raise
    /// them. A full decision set returns an empty vec and the loop continues.
    ///
    /// Precondition: call_id is unique across the session (minted at the
    /// provider boundary by unique_id_gen in openai_compat.rs). Decisions
    /// route by find on call_id; a duplicate id could attach an approval
    /// onto the wrong call — a safety, not just a display, failure. The mint
    /// makes it unreachable; this function does not re-defend.
    pub(crate) async fn apply_decisions(
        &self,
        session: SessionId,
        decisions: &[ApprovalDecision],
    ) -> Result<Vec<ApprovalRequest>, RunError> {
        let mut remaining = Vec::new();
        for req in self.pending_approvals(session).await? {
            let Some(decision) = decisions.iter().find(|d| d.call_id == req.call_id) else {
                // No decision for this call: leave it pending so a later
                // resume can decide it. Re-surface it to the caller.
                remaining.push(req);
                continue;
            };
            if decision.approved {
                if let Some(tool) = self.tools.get(&req.tool_name).cloned() {
                    // execute_authorized honors a Yes (guarded tools proceed past
                    // Ask) and still blocks a tightened Deny at enforcement. A
                    // decision may carry an updated input (AskUserQuestion
                    // answers collected by the UI); use it so they reach the tool.
                    let input = decision.updated_input.clone().unwrap_or(req.input.clone());
                    let result = tool
                        .execute_authorized(
                            ToolCtx::new(req.call_id.as_str()).with_session(session),
                            input,
                        )
                        .await;
                    let output = match result {
                        Ok(v) => v,
                        Err(e) => tool_error_json(&e),
                    };
                    self.append_tool_result(
                        session,
                        req.call_id.clone(),
                        &req.tool_name,
                        output,
                        0,
                    )
                    .await?;
                } else {
                    self.append_tool_result(
                        session,
                        req.call_id.clone(),
                        &req.tool_name,
                        SyntheticToolOutcome::UnknownTool {
                            name: req.tool_name.clone(),
                            on_resume: true,
                        }
                        .to_json(),
                        0,
                    )
                    .await?;
                }
            } else {
                self.append_tool_result(
                    session,
                    req.call_id.clone(),
                    &req.tool_name,
                    SyntheticToolOutcome::Rejected.to_json(),
                    0,
                )
                .await?;
            }
        }
        Ok(remaining)
    }

    /// Reconcile: append an interrupted-by-user result for every ToolCall with
    /// no matching ToolResult. Matches the reject branch so the session stays
    /// lossless after an abort (no orphan ToolCall without a result).
    pub(crate) async fn reconcile_tool_results(&self, session: SessionId) -> Result<(), RunError> {
        for req in self.pending_approvals(session).await? {
            self.append_tool_result(
                session,
                req.call_id.clone(),
                &req.tool_name,
                SyntheticToolOutcome::Interrupted.to_json(),
                0,
            )
            .await?;
        }
        Ok(())
    }

    /// The approval requests pending for a session: ToolCall events whose
    /// call_id has no matching ToolResult yet. Used by resume() to know which
    /// calls to execute. (scans the replay; a real impl indexes this.)
    ///
    /// Precondition: call_id is unique across the session (minted at the
    /// provider boundary by unique_id_gen in openai_compat.rs). The answered
    /// set keys on call_id; a duplicate id would let one ToolResult mark
    /// every same-id call answered (silently dropping a pending call). The
    /// mint makes it unreachable; this function does not re-defend.
    async fn pending_approvals(
        &self,
        session: SessionId,
    ) -> Result<Vec<ApprovalRequest>, RunError> {
        let events = self.store.replay(session).await?;
        let mut answered = std::collections::HashSet::new();
        for e in &events {
            if let TurnEventKind::ToolResult { call_id, .. } = &e.kind {
                answered.insert(call_id.clone());
            }
        }
        let mut pending = Vec::new();
        for e in &events {
            if let TurnEventKind::ToolCall {
                call_id,
                tool,
                input,
            } = &e.kind
                && !answered.contains(call_id)
            {
                pending.push(ApprovalRequest::new(
                    call_id.clone(),
                    tool.clone(),
                    input.clone(),
                ));
            }
        }
        Ok(pending)
    }
}
