//! Mid-run + between-run session-notification helpers. Split from server.rs
//! so that file stays under the file-size gate. Both are Server methods the
//! serve loop + the resume select call on receiving a session/* JSON-RPC
//! notification mid-run, plus the permission-mode cycle request that rides
//! the same mid-run select arm.

use std::sync::Arc;

use houyicoder_protocol::envelope::{
    RequestEnvelope, ResponseEnvelope, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;

use super::{Server, io::ServerIo};

impl Server {
    /// Handle a permission-mode-cycle request received mid-run (Shift+Tab).
    /// The gate is Mutex-protected; the drive loop reads it at decide() time,
    /// so the switch is immediate for the next tool call. Replies with the new
    /// mode so the frontend's /mode view stays in sync without a separate
    /// poll. Other request payloads are dropped (no other mid-run verb today).
    pub(super) async fn handle_mode_cycle_during_run(
        gate: &Arc<dyn houyicoder_permission::ModeGate>,
        io: &mut ServerIo,
        req: RequestEnvelope,
    ) {
        let payload = match req.payload {
            FrontendRequest::PermissionCycleMode => match gate.tab_cycle() {
                Ok(mode) => Some(ResponsePayload::PermissionMode(
                    crate::projection::project_permission_mode(mode),
                )),
                Err(_) => None,
            },
            _ => None,
        };
        if let Some(payload) = payload {
            let frame = ServerFrame::Response(ResponseEnvelope::new(req.req_id, payload));
            if let Ok(encoded) = encode(&frame) {
                drop(io.send_frame(encoded).await);
            }
        }
    }

    /// Handle a session/* JSON-RPC notification received mid-run or
    /// between runs. session/cancel aborts the in-flight run; session/inject
    /// enqueues a user message for mid-turn injection (the drive loop drains
    /// it at the next turn boundary); session/queue_remove drops a queued
    /// message by text (overlay delete, or the frontend popping the head to
    /// start a follow-up run so the new run does not re-inject it). All three
    /// are fire-and-forget (no reply) — the effect shows up in the run's
    /// outcome + transcript. No-op when the text is not in the queue.
    pub(super) fn handle_session_notification(
        &self,
        notif: &houyicoder_protocol::acp_wire::AcpNotification,
    ) {
        match notif.method.as_str() {
            "session/cancel" => self.runner.abort(),
            "session/inject" => {
                let text = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !text.is_empty() {
                    self.runner.enqueue_input(text);
                }
            }
            "session/queue_remove" => {
                if let Some(text) = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("text"))
                    .and_then(|v| v.as_str())
                {
                    self.runner.remove_input(text);
                }
            }
            _ => {}
        }
    }
}
