//! Routing a child agent's permission ask through the host's approval flow,
//! shared by the frontend server and the ACP server. A child publishes a
//! PermissionRequest on the bus; the host surfaces it as one approval card,
//! settles the answer through the consent policy, and publishes the decision
//! on the per-request response topic the child is awaiting. A host that drops
//! while a delegated ask is pending publishes a deny so the child resumes
//! instead of waiting for a response that will not come.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::ApprovalRequest;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, permission_request_topic, permission_response_topic,
};
use houyicoder_protocol::frontend::run::DelegationSource;
use tokio::sync::broadcast::Receiver;

/// Connection-scoped receiver for child permission asks, shared across idle
/// waits and active turns so every published ask is consumed once per host.
#[derive(Default)]
pub(crate) struct DelegatedAskReceiver {
    slot: Mutex<Option<Receiver<BusMessage>>>,
    subscribed: AtomicBool,
}

impl DelegatedAskReceiver {
    pub(crate) fn lease(&self, bus: Option<&Arc<AgentBus>>) -> PermissionLease<'_> {
        let rx = if !self.subscribed.swap(true, Ordering::AcqRel) {
            bus.map(|b| b.subscribe(permission_request_topic()))
        } else {
            self.slot.lock().ok().and_then(|mut s| s.take())
        };
        PermissionLease {
            owner: &self.slot,
            rx,
        }
    }
}

/// Drop guard that returns the shared broadcast receiver to its slot on every
/// exit path, including cancellation at an await point.
pub(crate) struct PermissionLease<'a> {
    owner: &'a Mutex<Option<Receiver<BusMessage>>>,
    rx: Option<Receiver<BusMessage>>,
}

impl PermissionLease<'_> {
    pub(crate) fn receiver(&mut self) -> Option<&mut Receiver<BusMessage>> {
        self.rx.as_mut()
    }
}

impl Drop for PermissionLease<'_> {
    fn drop(&mut self) {
        if let Some(rx) = self.rx.take()
            && let Ok(mut slot) = self.owner.lock()
        {
            *slot = Some(rx);
        }
    }
}

/// One child ask prepared for the host's approval flow: the engine request
/// reconstructed from the bus message, and the delegation label that marks
/// the card with its origin.
pub(crate) struct DelegatedAsk {
    pub(crate) child_id: String,
    pub(crate) call_id: String,
    pub(crate) approval: ApprovalRequest,
    pub(crate) delegation: DelegationSource,
}

/// Reconstruct the approval request from a child's bus message and label it
/// with its origin. None when the message is not a permission request, or the
/// child is no longer waiting on this ask (a stale request a reconnecting host
/// should not re-answer).
pub(crate) fn prepare_delegated_ask(runner: &Runner, req: BusMessage) -> Option<DelegatedAsk> {
    let BusMessage::PermissionRequest {
        child_id,
        subagent_type,
        call_id,
        tool,
        input,
        reason,
    } = req
    else {
        return None;
    };
    // A runner without a spawn handle has no child registry to consult, so the
    // ask is not stale: proceed. A runner with one proceeds only while the
    // child is still waiting on this call, so a reconnecting host does not
    // re-answer an ask the child already abandoned.
    let waiting = match runner.spawn_handle() {
        Some(handle) => handle
            .take_pending_child_permissions()
            .iter()
            .any(|p| p.child_id == child_id && p.call_id == call_id),
        None => true,
    };
    if !waiting {
        return None;
    }
    let approval = ApprovalRequest::new(call_id.clone(), tool, input, reason);
    let delegation = DelegationSource {
        child_id: child_id.clone(),
        subagent_type,
    };
    Some(DelegatedAsk {
        child_id,
        call_id,
        approval,
        delegation,
    })
}

/// Publish the parent's decision for one child ask on the per-request response
/// topic the child subscribed to before it published the request.
pub(crate) fn publish_delegated_response(
    bus: Option<&Arc<AgentBus>>,
    child_id: &str,
    call_id: &str,
    approved: bool,
    updated_input: Option<serde_json::Value>,
) {
    let Some(bus) = bus else {
        return;
    };
    bus.publish(
        &permission_response_topic(child_id, call_id),
        BusMessage::PermissionResponse {
            call_id: call_id.to_string(),
            approved,
            updated_input,
            scope: "once".to_string(),
        },
    );
}

/// Fail-closed for a delegated ask the host could not answer: publish a deny
/// so the child resumes (rejected) rather than hanging on a response that will
/// not come.
pub(crate) fn deny_delegated_ask(bus: Option<&Arc<AgentBus>>, child_id: &str, call_id: &str) {
    publish_delegated_response(bus, child_id, call_id, false, None);
}
