//! Parent-side bridge for a child's permission ask: a PermissionRequest on the
//! bus is routed through the server's existing wire-approval flow, and the
//! decision is published back on the per-request response topic so the child
//! resumes. Mirrors the parent's own approval path (handle_approval); a
//! child ask is just another wire Permission the TUI answers.

use std::sync::Arc;

use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::ApprovalRequest;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, permission_response_topic,
};

use super::frame_carrier::FrameCarrier;
use super::{ProtocolError, Server};

impl Server {
    /// Wire the shared multi-agent bus so a child's permission ask reaches
    /// the wire-approval flow. None on the single-agent path.
    pub fn with_bus(mut self, bus: Option<Arc<AgentBus>>) -> Self {
        self.bus = bus;
        self
    }
}

/// Handle one child permission request: reconstruct the ApprovalRequest,
/// drive it through the wire-approval flow (the TUI answers), and publish the
/// decision on the per-request response topic the child is awaiting. On a
/// wire failure (client closed mid-ask) publish a deny so the child resumes
/// instead of hanging on a response that will not come.
pub(crate) async fn handle(
    server: &mut Server,
    io: &mut FrameCarrier,
    req: BusMessage,
) -> Result<(), ProtocolError> {
    let BusMessage::PermissionRequest {
        child_id,
        subagent_type,
        call_id,
        tool,
        input,
        ..
    } = req
    else {
        // A non-request message on the permission topic: ignore.
        return Ok(());
    };
    let approval = ApprovalRequest::new(call_id.clone(), tool.clone(), input.clone());
    // Label the ask with its origin so the TUI card shows which delegation
    // the ask belongs to (a child ask vs the parent's own tool call).
    let delegation = houyicoder_protocol::frontend::run::DelegationSource {
        child_id: child_id.clone(),
        subagent_type: subagent_type.clone(),
    };
    let (approved, updated_input) = match server
        .handle_approval(io, &approval, Some(delegation))
        .await
    {
        Ok(d) => (d.approved, d.updated_input),
        Err(e) => {
            // Fail-closed: publish a deny so the child resumes (rejected)
            // rather than hanging. The error still propagates so the caller
            // can surface a wire failure.
            if let Some(bus) = server.bus.as_ref() {
                bus.publish(
                    &permission_response_topic(&child_id, &call_id),
                    BusMessage::PermissionResponse {
                        call_id,
                        approved: false,
                        updated_input: None,
                        scope: "once".to_string(),
                    },
                );
            }
            return Err(e);
        }
    };
    if let Some(bus) = server.bus.as_ref() {
        bus.publish(
            &permission_response_topic(&child_id, &call_id),
            BusMessage::PermissionResponse {
                call_id,
                approved,
                updated_input,
                // The scope chosen at the TUI is applied by handle_approval's
                // consent routing on the shared gate; the child's resume only
                // reads approved + updated_input.
                scope: "once".to_string(),
            },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
