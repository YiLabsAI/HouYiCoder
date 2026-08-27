//! Mid-run + between-run session-notification helpers. Split from server.rs
//! so that file stays under the file-size gate. Both are Server methods the
//! serve loop + the resume select call on receiving a session/* JSON-RPC
//! notification mid-run, plus the mid-run request handler that the same
//! select arm routes through.

use houyicoder_protocol::envelope::{
    RequestEnvelope, ResponseEnvelope, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;

use super::{Server, io::ServerIo};

impl Server {
    /// Handle a request received mid-run. Two payloads are safe to process
    /// while a turn is in flight: the permission-mode cycle (Shift+Tab)
    /// updates the Mutex-protected gate the drive loop reads at decide() time,
    /// so the switch lands before the next tool call; the child transcript
    /// fetch is a read-only store query (replay + project, no side effects)
    /// so it does not race the parent run. Other payloads are dropped (no
    /// other mid-run verb today); they ride this arm because a mid-run client
    /// frame parses as a Request but most verbs mutate state and must wait.
    pub(super) async fn handle_request_during_run(&self, io: &mut ServerIo, req: RequestEnvelope) {
        let req_id = req.req_id;
        match req.payload {
            FrontendRequest::PermissionCycleMode => {
                let Ok(mode) = self.gate.tab_cycle() else {
                    return;
                };
                let frame = ServerFrame::Response(ResponseEnvelope::new(
                    req_id,
                    ResponsePayload::PermissionMode(crate::projection::project_permission_mode(
                        mode,
                    )),
                ));
                // Encode failure is survivable: skip the frame and keep the
                // serve loop alive rather than panicking the task, matching
                // the between-run dispatch path's graceful error return.
                if let Ok(encoded) = encode(&frame) {
                    drop(io.send_frame(encoded).await);
                }
            }
            FrontendRequest::ChildTranscript { child_sid } => {
                let frames = self.child_transcript_frames(&child_sid).await;
                let frame = ServerFrame::Response(ResponseEnvelope::new(
                    req_id,
                    ResponsePayload::ChildTranscript { child_sid, frames },
                ));
                if let Ok(encoded) = encode(&frame) {
                    drop(io.send_frame(encoded).await);
                }
            }
            _ => {}
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
            "session/inject_child" => {
                // Steering: route a user's text into a running child's inbox
                // (the teammate-view input path). The child's drive loop drains
                // it at the next turn boundary. Fire-and-forget; a missing
                // multi-agent runtime or unregistered child logs + drops.
                let Some(child_sid) = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("childSid"))
                    .and_then(|v| v.as_str())
                else {
                    return;
                };
                let text = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if text.is_empty() {
                    return;
                }
                if let Err(e) = self.runner.steer_child(child_sid, text) {
                    tracing::warn!("inject_child: {e}");
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_protocol::acp_wire::AcpNotification;
    use std::sync::Arc;

    fn server_no_runtime() -> Server {
        let store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        )));
        let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
            Arc::new(houyicoder_provider::FakeProvider::text("x"));
        let runner = houyicoder_core::agent::Runner::with_shared_store(
            store,
            provider,
            houyicoder_core::agent::ToolRegistry::new(),
            houyicoder_core::agent::runner_config::RunnerConfig::default(),
        );
        Server::new(
            Arc::new(runner),
            houyicoder_context::SessionId::new(),
            Arc::new(houyicoder_permission::DefaultModeGate::new()),
        )
    }

    /// Without a multi-agent runtime the handler logs the Err + drops (no
    /// panic, no reply). The runner has no spawn handle, so steer_child
    /// returns Err — the no-bus path on a single-agent server.
    #[tokio::test]
    async fn test_inject_child_no_runtime() {
        let server = server_no_runtime();
        server.handle_session_notification(&AcpNotification::new(
            "session/inject_child",
            serde_json::json!({ "childSid": "c1", "text": "x" }),
        ));
    }

    /// A missing childSid param is a silent no-op (no panic on unwrap).
    #[tokio::test]
    async fn test_inject_child_missing_sid() {
        let server = server_no_runtime();
        server.handle_session_notification(&AcpNotification::new(
            "session/inject_child",
            serde_json::json!({ "text": "x" }),
        ));
    }
}
