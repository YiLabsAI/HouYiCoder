//! Foreground child execution and permission routing.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use houyicoder_api::spawn::PendingChildPermission;
use houyicoder_async::CancellationToken;
use houyicoder_async::bus::MessageBus;
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, permission_request_topic, permission_response_topic,
};
use houyicoder_core::agent::{ApprovalDecision, ApprovalRequest, RunError, RunOutcome, RunResult};
use tokio::time::sleep;

/// How long a child waits for a human verdict before fail-closed deny.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// The parent-side routing a child uses to reach the approval flow: the bus
/// that carries the ask, and the park that survives a client disconnect so a
/// reconnecting serve loop can re-emit the card.
pub(super) struct ParentContact {
    pub bus: Option<Arc<AgentBus>>,
    pub pending_permissions: Arc<Mutex<Vec<PendingChildPermission>>>,
}

/// Drive a foreground child to a terminal state, routing mid-run permission
/// asks through the bus to the parent's approval flow. A child with no bus
/// takes the headless path. A cancel ends the child at a terminal status.
pub(super) async fn drive_child_to_terminal(
    runner: Arc<Runner>,
    session: SessionId,
    task: String,
    cancel: CancellationToken,
    contact: ParentContact,
    child_id: &str,
    agent_type: &str,
) -> Result<RunResult, RunError> {
    let mut run = Box::pin(runner.run(session, task));
    // A stop aborts the run and then lets it finish: the run reconciles the
    // tool results its turn already dispatched and reports Interrupted, which
    // is a terminal status the fleet clears the child's row on. The run arm
    // comes first so a run that has not been polled yet still starts — its
    // cancel token is minted on the first poll, and abort cannot reach it
    // before then.
    let mut result = tokio::select! {
        biased;
        res = &mut run => res,
        _ = cancel.cancelled() => {
            runner.abort();
            run.await
        }
    };
    loop {
        let approvals = match &result {
            Ok(r) => match &r.outcome {
                RunOutcome::Interruption(a) => a.clone(),
                _ => return result,
            },
            Err(_) => return result,
        };
        // No bus: headless. The Interruption result reaches the caller's
        // terminal_summary, which maps it to "interrupted".
        let Some(bus) = contact.bus.as_ref() else {
            return result;
        };
        let decisions = match route_approvals_via_bus(
            bus,
            child_id,
            agent_type,
            approvals,
            &cancel,
            &contact.pending_permissions,
        )
        .await
        {
            Some(d) => d,
            // The ask was cancelled: the run is parked on a decision that
            // will not arrive, so abort it and resume with no decisions.
            // The resume short-circuits on the abort and reports
            // Interrupted — the terminal status the fleet clears the row on
            // — rather than leaving the child parked on the ask.
            None => {
                runner.abort();
                return runner.resume(session, &[]).await;
            }
        };
        result = {
            let mut run = Box::pin(runner.resume(session, &decisions));
            tokio::select! {
                biased;
                res = &mut run => res,
                _ = cancel.cancelled() => {
                    runner.abort();
                    run.await
                }
            }
        };
    }
}

/// Route a batch of approval requests through the bus: subscribe before
/// publish so no broadcast lag drops the decision, then await the response.
/// Cancel returns None (abort). A timeout produces a deny decision so the
/// child resumes rejected rather than hanging.
pub(super) async fn route_approvals_via_bus(
    bus: &Arc<AgentBus>,
    child_id: &str,
    agent_type: &str,
    approvals: Vec<ApprovalRequest>,
    cancel: &CancellationToken,
    pending: &Arc<Mutex<Vec<PendingChildPermission>>>,
) -> Option<Vec<ApprovalDecision>> {
    let mut decisions = Vec::with_capacity(approvals.len());
    for req in approvals {
        let call_id = req.call_id.clone();
        let mut rx = bus.subscribe(&permission_response_topic(child_id, &call_id));
        let parked = PendingChildPermission {
            child_id: child_id.to_string(),
            subagent_type: agent_type.to_string(),
            call_id: call_id.clone(),
            tool: req.tool_name.clone(),
            input: req.input.clone(),
            reason: req.reason.clone(),
        };
        if let Ok(mut v) = pending.lock() {
            v.push(parked);
        }
        bus.publish(
            permission_request_topic(),
            BusMessage::PermissionRequest {
                child_id: child_id.to_string(),
                subagent_type: agent_type.to_string(),
                call_id: call_id.clone(),
                tool: req.tool_name.clone(),
                input: req.input.clone(),
                reason: req.reason.clone(),
            },
        );
        let resp = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                clear_pending(pending, &call_id);
                return None;
            }
            _ = sleep(APPROVAL_TIMEOUT) => {
                clear_pending(pending, &call_id);
                decisions.push(ApprovalDecision {
                    call_id,
                    approved: false,
                    updated_input: None,
                });
                continue;
            }
            r = rx.recv() => r,
        };
        let Ok(resp) = resp else {
            clear_pending(pending, &call_id);
            return None;
        };
        let BusMessage::PermissionResponse {
            call_id: cid,
            approved,
            updated_input,
            ..
        } = resp
        else {
            clear_pending(pending, &call_id);
            return None;
        };
        clear_pending(pending, &cid);
        decisions.push(ApprovalDecision {
            call_id: cid,
            approved,
            updated_input,
        });
    }
    Some(decisions)
}

fn clear_pending(pending: &Arc<Mutex<Vec<PendingChildPermission>>>, call_id: &str) {
    if let Ok(mut v) = pending.lock() {
        v.retain(|p| p.call_id != call_id);
    }
}

/// Reconstruct the bus message a reconnecting serve loop re-emits.
pub(crate) fn permission_request_from(ask: PendingChildPermission) -> BusMessage {
    BusMessage::PermissionRequest {
        child_id: ask.child_id,
        subagent_type: ask.subagent_type,
        call_id: ask.call_id,
        tool: ask.tool,
        input: ask.input,
        reason: ask.reason,
    }
}

#[cfg(test)]
mod tests;
