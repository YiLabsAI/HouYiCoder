//! Drive a sync child to a terminal state, routing mid-run permission asks
//! through the bus to the parent's approval flow. Extracted so the runtime
//! file stays under the size gate.

use std::sync::Arc;

use houyicoder_async::CancellationToken;
use houyicoder_async::bus::MessageBus;
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, permission_request_topic, permission_response_topic,
};
use houyicoder_core::agent::{ApprovalDecision, ApprovalRequest, RunError, RunOutcome, RunResult};

/// Drive a sync child to a terminal state, routing mid-run permission asks
/// through the bus to the parent's approval flow. Mirrors the parent server's
/// serve loop: run() returns RunOutcome::Interruption(approvals) when a
/// guarded tool needs approval; each approval is published on the bus, the
/// parent responds on the per-request response topic, and resume(decisions)
/// continues the run. A child with no bus takes the headless path: the
/// Interruption result is returned as-is so the caller surfaces it as
/// interrupted (the parentless child has no host). Returns None when the run
/// is canceled mid-run or mid-ask.
///
/// The parent's serve-loop perm_rx is scoped to its inner run block; sync-only
/// spawn keeps that safe (no child publishes while the parent's own
/// Interruption breaks the block), but a background-spawn path must make
/// perm_rx long-lived across the whole serve loop or in-flight child asks are
/// lost to broadcast lag.
pub(super) async fn drive_child_to_terminal(
    runner: Arc<Runner>,
    session: SessionId,
    task: String,
    cancel: CancellationToken,
    bus: Option<Arc<AgentBus>>,
    child_id: &str,
    agent_type: &str,
) -> Option<Result<RunResult, RunError>> {
    let mut result = {
        let abort_runner = Arc::clone(&runner);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                abort_runner.abort();
                return None;
            }
            res = runner.run(session, task) => res,
        }
    };
    loop {
        let approvals = match &result {
            Ok(r) => match &r.outcome {
                RunOutcome::Interruption(a) => a.clone(),
                _ => return Some(result),
            },
            Err(_) => return Some(result),
        };
        // No bus: headless. The Interruption result reaches the caller's
        // terminal_summary, which maps it to "interrupted".
        let Some(bus) = bus.as_ref() else {
            return Some(result);
        };
        let decisions =
            match route_approvals_via_bus(bus, child_id, agent_type, approvals, &cancel).await {
                Some(d) => d,
                None => {
                    runner.abort();
                    return None;
                }
            };
        result = {
            let abort_runner = Arc::clone(&runner);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    abort_runner.abort();
                    return None;
                }
                res = runner.resume(session, &decisions) => res,
            }
        };
    }
}

/// Route a batch of approval requests through the bus: for each, subscribe to
/// the per-request response topic (before publish, so no broadcast lag can
/// drop the decision), emit a PermissionRequest, and await the matching
/// PermissionResponse. Cancelable: returns None if the child is aborted
/// mid-ask, so the caller surfaces interrupted rather than hanging on a
/// response the parent will not send.
pub(super) async fn route_approvals_via_bus(
    bus: &Arc<AgentBus>,
    child_id: &str,
    agent_type: &str,
    approvals: Vec<ApprovalRequest>,
    cancel: &CancellationToken,
) -> Option<Vec<ApprovalDecision>> {
    let mut decisions = Vec::with_capacity(approvals.len());
    for req in approvals {
        let call_id = req.call_id.clone();
        let mut rx = bus.subscribe(&permission_response_topic(child_id, &call_id));
        bus.publish(
            permission_request_topic(),
            BusMessage::PermissionRequest {
                child_id: child_id.to_string(),
                agent_type: agent_type.to_string(),
                call_id: call_id.clone(),
                tool: req.tool_name.clone(),
                input: req.input.clone(),
            },
        );
        let resp = tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            r = rx.recv() => r,
        };
        let Ok(resp) = resp else {
            return None;
        };
        let BusMessage::PermissionResponse {
            call_id: cid,
            approved,
            updated_input,
            ..
        } = resp
        else {
            // A non-response on the per-request topic is a protocol error;
            // fail closed (child interrupted) rather than resuming with a
            // missing decision.
            return None;
        };
        decisions.push(ApprovalDecision {
            call_id: cid,
            approved,
            updated_input,
        });
    }
    Some(decisions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_async::CancellationToken;
    use std::sync::Arc;

    /// route_approvals_via_bus round-trips a permission ask to a parent
    /// responder + collects the decision. Pins the contract
    /// drive_child_to_terminal relies on: subscribe-before-publish on the
    /// per-call_id response topic so no broadcast lag drops the decision.
    #[tokio::test]
    async fn test_route_approvals_via_bus() {
        let bus = Arc::new(AgentBus::new());
        let mut parent_rx = bus.subscribe(permission_request_topic());
        let bus_for_parent = Arc::clone(&bus);
        let parent = tokio::spawn(async move {
            let req = parent_rx.recv().await.expect("parent got the ask");
            let BusMessage::PermissionRequest {
                child_id, call_id, ..
            } = req
            else {
                panic!("expected PermissionRequest")
            };
            bus_for_parent.publish(
                &permission_response_topic(&child_id, &call_id),
                BusMessage::PermissionResponse {
                    call_id,
                    approved: true,
                    updated_input: None,
                    scope: "once".to_string(),
                },
            );
        });
        let cancel = CancellationToken::new();
        let approvals = vec![ApprovalRequest::new(
            "call-1".into(),
            "bash".into(),
            serde_json::json!({"command": "echo hi"}),
        )];
        let decisions = route_approvals_via_bus(&bus, "child-1", "explore", approvals, &cancel)
            .await
            .expect("not canceled");
        assert_eq!(decisions.len(), 1);
        assert!(decisions[0].approved);
        assert_eq!(decisions[0].call_id, "call-1");
        parent.await.expect("parent responder done");
    }

    /// A canceled route returns None rather than hanging on a response the
    /// parent will not send.
    #[tokio::test]
    async fn test_route_approvals_cancel_none() {
        let bus = Arc::new(AgentBus::new());
        let cancel = CancellationToken::new();
        let approvals = vec![ApprovalRequest::new(
            "call-1".into(),
            "bash".into(),
            serde_json::json!({}),
        )];
        let cancel_for_task = cancel.clone();
        let task = tokio::spawn(async move {
            route_approvals_via_bus(&bus, "child-1", "explore", approvals, &cancel_for_task).await
        });
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(
            task.await.expect("task done").is_none(),
            "canceled route returns None, not a hang"
        );
    }
}
