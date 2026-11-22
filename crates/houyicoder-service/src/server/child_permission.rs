//! Parent-side bridge for a child's permission ask: a PermissionRequest on the
//! bus is routed through the server's existing wire-approval flow, and the
//! decision is published back on the per-request response topic so the child
//! resumes. The prepare/publish split lives in the consent kernel so the ACP
//! server shares it; this module holds the server's own ask path.

use std::sync::Arc;

use houyicoder_core::agent::multi_agent::bus_types::{AgentBus, BusMessage};

use crate::consent::{deny_delegated_ask, prepare_delegated_ask, publish_delegated_response};

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

/// Handle one child permission request: reconstruct the approval request,
/// drive it through the wire-approval flow (the TUI answers), and publish the
/// decision on the per-request response topic the child is awaiting. On a
/// wire failure (client closed mid-ask) publish a deny so the child resumes
/// instead of hanging on a response that will not come.
pub(crate) async fn handle(
    server: &mut Server,
    io: &mut FrameCarrier,
    req: BusMessage,
) -> Result<(), ProtocolError> {
    let Some(ask) = prepare_delegated_ask(&server.runner, req) else {
        return Ok(());
    };
    let result = server
        .handle_approval(io, &ask.approval, Some(ask.delegation))
        .await;
    let bus = server.bus.as_ref();
    match result {
        Ok(d) => {
            publish_delegated_response(
                bus,
                &ask.child_id,
                &ask.call_id,
                d.approved,
                d.updated_input,
            );
            Ok(())
        }
        Err(e) => {
            deny_delegated_ask(bus, &ask.child_id, &ask.call_id);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests;
