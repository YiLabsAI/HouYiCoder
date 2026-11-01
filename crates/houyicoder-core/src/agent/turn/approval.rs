//! Approval flow for Runner: resolve a caller's decisions onto the pending
//! tool calls, reconcile orphan calls on abort, and enumerate which calls
//! still await a decision. Extracted from the main impl so the approval
//! path and its call_id-uniqueness invariant live together.

use houyicoder_api::skill::GrantSubject;
use houyicoder_api::skill::grant::SkillGrantStore;
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::fallback::FallbackToolOutcome;
use crate::agent::turn::resolve::resolved_input;
use crate::agent::turn::withhold::{self, CheckAction, CheckPlan, EditOutcome};
use crate::agent::{ApprovalDecision, ApprovalRequest, RunError, Runner};
use std::collections::HashMap;

/// One call of a model response whose release is being settled: a call that
/// still awaits a verdict, or an earlier call of the same response with what
/// the log already records about it.
struct BatchCall {
    request: Option<ApprovalRequest>,
    tool: String,
    outcome: EditOutcome,
}

/// What releasing parked calls left behind, folded into the session tallies
/// once every response is settled.
#[derive(Default)]
struct BatchSettlement {
    results: Vec<(String, Value)>,
    call_names: HashMap<String, String>,
    /// Set once a stop lands while an approved call is running; every call
    /// after it is recorded interrupted instead of released.
    stopped: bool,
}

impl Runner {
    /// Apply decisions to pending tool calls and return requests that still
    /// lack a decision. Unmatched requests remain pending for a later resume.
    ///
    /// Requires call_id to be unique within the session. Decisions route only
    /// by call_id, so a duplicate could authorize the wrong call. IDs are
    /// minted at the provider boundary; this function does not revalidate them.
    pub(crate) async fn apply_decisions(
        &self,
        session: SessionId,
        decisions: &[ApprovalDecision],
        token: &CancellationToken,
    ) -> Result<Vec<ApprovalRequest>, RunError> {
        let mut remaining = Vec::new();
        let mut settled = BatchSettlement::default();
        for batch in self.pending_state(session).await? {
            let still_pending = self
                .settle_batch(session, batch, decisions, token, &mut settled)
                .await?;
            remaining.extend(still_pending);
        }
        // Folded once, after every response is settled, so a call released
        // here counts like one the live turn dispatched.
        self.fold_tool_outcomes(&settled.results, &settled.call_names);
        Ok(remaining)
    }

    /// Settle one response's parked calls against the decisions, returning
    /// the requests that still await one.
    async fn settle_batch(
        &self,
        session: SessionId,
        batch: Vec<BatchCall>,
        decisions: &[ApprovalDecision],
        token: &CancellationToken,
        settled: &mut BatchSettlement,
    ) -> Result<Vec<ApprovalRequest>, RunError> {
        let mut remaining = Vec::new();
        // The same check rule the live turn applies, seeded from the log: the
        // response that parked these calls may already record that an edit
        // before them wrote nothing. Reaching this path means an earlier
        // process stopped before answering every call of the response, or a
        // session written by an older build left a call unanswered.
        let mut checks = CheckPlan::default();
        for call in batch {
            let Some(req) = call.request else {
                checks.observe(&call.tool, call.outcome);
                continue;
            };
            settled
                .call_names
                .insert(req.call_id.clone(), req.tool_name.clone());
            if settled.stopped {
                // The stop landed while an approved call was running: the
                // calls behind it do not start. Each is recorded interrupted
                // so the session stays lossless and a later resume cannot run
                // a batch the user already stopped.
                let output = self
                    .record_interrupted(session, &req.call_id, &req.tool_name, &req.input)
                    .await?;
                settled.results.push((req.call_id, output));
                continue;
            }
            match checks.action(&req.tool_name) {
                CheckAction::Skip => {
                    // The edit it follows wrote nothing, so the check is
                    // answered now rather than offered on a card.
                    let output = self
                        .answer_skipped(session, &req.call_id, &req.tool_name)
                        .await?;
                    settled.results.push((req.call_id, output));
                    continue;
                }
                CheckAction::Wait => {
                    // The edit it follows is still undecided: keep the check
                    // for the resume that decides that edit.
                    remaining.push(req);
                    continue;
                }
                CheckAction::Run => {}
            }
            let Some(decision) = decisions.iter().find(|d| d.call_id == req.call_id) else {
                // No decision for this call: leave it pending so a later
                // resume can decide it, and return it to the caller so the
                // card is offered again. An edit left undecided may still
                // write, so the checks after it wait on this verdict rather
                // than running now.
                checks.observe(&req.tool_name, EditOutcome::Pending);
                remaining.push(req);
                continue;
            };
            if decision.approved {
                // Entitlement approval: write services to the grant store
                // instead of executing a tool. Nothing runs, so it is not a
                // tool outcome and does not enter the tally. The services were
                // discovered by the deny-log scan after a failed bash command.
                if req.tool_name == ENTITLEMENT_TOOL {
                    let output = self.apply_entitlement_grant(&req.input);
                    self.append_tool_result(
                        session,
                        req.call_id.clone(),
                        &req.tool_name,
                        output,
                        0,
                    )
                    .await?;
                    continue;
                }
                if let Some(tool) = self.tools.get(&req.tool_name).cloned() {
                    // A decision may carry an updated input (the answers
                    // AskUserQuestion collected in the UI); use it so they
                    // reach the tool.
                    let input = decision.updated_input.clone().unwrap_or(req.input.clone());
                    let output = self
                        .execute_approved_call(session, req.call_id.as_str(), &tool, input, token)
                        .await?;
                    checks.observe(&req.tool_name, withhold::recorded_outcome(&output));
                    settled.results.push((req.call_id.clone(), output));
                    settled.stopped = token.is_cancelled();
                } else {
                    checks.observe(&req.tool_name, EditOutcome::Missed);
                    let output = FallbackToolOutcome::UnknownTool {
                        name: req.tool_name.clone(),
                        on_resume: true,
                    }
                    .to_json();
                    self.append_tool_result(
                        session,
                        req.call_id.clone(),
                        &req.tool_name,
                        output.clone(),
                        0,
                    )
                    .await?;
                    settled.results.push((req.call_id, output));
                }
            } else {
                // A declined edit did not run either, so its checks stay
                // withdrawn.
                checks.observe(&req.tool_name, EditOutcome::Missed);
                let output = FallbackToolOutcome::Rejected.to_json();
                self.append_tool_result(
                    session,
                    req.call_id.clone(),
                    &req.tool_name,
                    output.clone(),
                    0,
                )
                .await?;
                settled.results.push((req.call_id, output));
            }
        }
        Ok(remaining)
    }

    /// Answer a shell call the batch must not run, because an edit it follows
    /// did not complete. The answer is recorded under the call's own id and
    /// tool name, and it is durable, so the call is not left unanswered and no
    /// later resume can offer it again.
    async fn answer_skipped(
        &self,
        session: SessionId,
        call_id: &str,
        tool_name: &str,
    ) -> Result<Value, RunError> {
        let output = withhold::skipped_after_edit();
        self.append_tool_result(session, call_id.to_string(), tool_name, output.clone(), 0)
            .await?;
        Ok(output)
    }

    /// Write discovered mach services to the grant store so the next skill
    /// invocation includes them. Returns a JSON result the model sees.
    fn apply_entitlement_grant(&self, input: &serde_json::Value) -> serde_json::Value {
        apply_entitlement(input, self.skill_grants.as_deref())
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
                FallbackToolOutcome::Interrupted.to_json(),
                0,
            )
            .await?;
        }
        Ok(())
    }

    /// The calls a session still owes a verdict on: ToolCall events whose call_id
    /// has no matching ToolResult yet. The abort reconciliation answers each of
    /// them, so an aborted run leaves no orphan call. (scans the replay; a real
    /// impl indexes this.)
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
        Ok(self
            .pending_state(session)
            .await?
            .into_iter()
            .flatten()
            .filter_map(|call| call.request)
            .collect())
    }

    /// Replay the session once and return every model response that still has
    /// an unanswered call, each with what the log records about the rest of
    /// that response's calls.
    ///
    /// A response's calls are the ToolCalls between its AssistantMessage and
    /// the next one. The batches stay separate because the check rule is
    /// settled per response: an edit in an earlier response says nothing
    /// about the source a later response's check reads.
    async fn pending_state(&self, session: SessionId) -> Result<Vec<Vec<BatchCall>>, RunError> {
        let events = self.store.replay(session).await?;
        let mut answered: HashMap<String, EditOutcome> = HashMap::new();
        for e in &events {
            if let SessionEvent::ToolResult {
                call_id, output, ..
            } = &e.event
            {
                answered.insert(call_id.clone(), withhold::recorded_outcome(output));
            }
        }
        let mut batches: Vec<Vec<BatchCall>> = Vec::new();
        for e in &events {
            match &e.event {
                SessionEvent::AssistantMessage { .. } => batches.push(Vec::new()),
                SessionEvent::ToolCall {
                    call_id,
                    tool,
                    input,
                } => {
                    let recorded = answered.get(call_id).copied();
                    let request = match recorded {
                        // A recovered call is resolved to the text the host
                        // runs, the same way it is resolved before a call is
                        // judged, so the approval on resume offers the command
                        // that will execute.
                        None => Some(ApprovalRequest::new(
                            call_id.clone(),
                            tool.clone(),
                            resolved_input(self.tools.get(tool), input),
                            // The gate's verdict happened in an earlier
                            // process and the record does not store it, so the
                            // card offers a generic prompt rather than a
                            // verdict taken now against a call it no longer
                            // judges.
                            None,
                        )),
                        Some(_) => None,
                    };
                    let outcome = recorded.unwrap_or(EditOutcome::Pending);
                    let call = BatchCall {
                        request,
                        tool: tool.clone(),
                        outcome,
                    };
                    match batches.last_mut() {
                        Some(batch) => batch.push(call),
                        None => batches.push(vec![call]),
                    }
                }
                _ => {}
            }
        }
        Ok(batches
            .into_iter()
            .filter(|batch| batch.iter().any(|call| call.request.is_some()))
            .collect())
    }
}

/// Merge discovered services into the grant store and return a JSON
/// result the model sees. Pure of runner state — takes the store by ref.
fn apply_entitlement(input: &Value, store: Option<&SkillGrantStore>) -> Value {
    let skill = input.get("skill").and_then(|v| v.as_str()).unwrap_or("");
    if skill.is_empty() {
        return serde_json::json!({ "error": "no skill named", "granted": [] });
    }
    let subject = input.get("grant_subject").and_then(GrantSubject::from_json);
    let Some(subject) = subject.filter(|subject| subject.skill == skill) else {
        return serde_json::json!({
            "error": "invalid grant subject",
            "skill": skill,
            "granted": [],
        });
    };
    let services: Vec<String> = input
        .get("services")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if let Some(store) = store {
        match store.add_grants(&subject, services) {
            Ok(granted) => {
                let count = granted.len();
                serde_json::json!({
                    "granted": granted,
                    "skill": skill,
                    "message": format!("Authorized {count} service(s) for {skill}."),
                })
            }
            Err(e) => serde_json::json!({
                "error": format!("grant persistence failed: {e}"),
                "skill": skill,
                "granted": [],
            }),
        }
    } else {
        serde_json::json!({ "error": "grant store not wired", "skill": skill })
    }
}

#[cfg(test)]
mod tests;
