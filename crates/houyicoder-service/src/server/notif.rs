//! Mid-run + between-run session-notification helpers. Split from server.rs
//! so that file stays under the file-size gate. Both are Server methods the
//! serve loop + the resume select call on receiving a session/* JSON-RPC
//! notification mid-run, plus the mid-run request handler that the same
//! select arm routes through.

use houyicoder_protocol::envelope::{
    RequestEnvelope, RequestId, ResponseEnvelope, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::model::EffectiveFrom;

use super::Server;
use super::frame_carrier::FrameCarrier;
use super::model_apply::ModelSelection;
use crate::protocol_adapter::permission_mode_to_wire;
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::frontend::QueuedInput;

impl Server {
    /// Handle a request received mid-run. The payloads safe to process while
    /// a turn is in flight are the ones that either read state or take effect
    /// at a boundary: the permission-mode cycle (Shift+Tab) updates the
    /// Mutex-protected gate the drive loop reads at decide() time, so the
    /// switch lands before the next tool call; the child transcript fetch and
    /// the model query are read-only mappings (no side effects) so they do
    /// not race the parent run; the model set routes through the same apply
    /// path the between-runs dispatch uses, so a pick submitted while a turn
    /// is in flight takes effect on the next request rather than being
    /// dropped. The in-flight request already built its body from the old
    /// model, so the switch reports NextRequest; other payloads ride this arm
    /// because a mid-run client frame parses as a Request but most verbs
    /// mutate state that must wait.
    pub(super) async fn handle_request_during_run(
        &mut self,
        io: &mut FrameCarrier,
        req: RequestEnvelope,
    ) {
        let req_id = req.req_id;
        match req.payload {
            FrontendRequest::PermissionCycleMode => {
                let payload = match self.gate.tab_cycle() {
                    Ok(mode) => ResponsePayload::PermissionMode(permission_mode_to_wire(mode)),
                    // A cycle the gate refuses is still an answer. Returning
                    // without one leaves the client waiting for a response it
                    // will never get, which is the hazard this handler exists
                    // to remove.
                    Err(e) => ResponsePayload::Error(ProtocolError::new(
                        ErrorCategory::InvalidRequest,
                        format!("permission mode cycle refused: {e}"),
                        true,
                    )),
                };
                reply(io, req_id, payload).await;
            }
            // A cancel is a request the run itself is waiting for, so it is
            // handled rather than deferred: deferring it would refuse the verb
            // exactly when the user needs it.
            FrontendRequest::RunCancel { session_id, .. } => {
                if let Err(e) = self.dispatch_run_cancel(io, req_id, session_id).await {
                    reply(io, req_id, ResponsePayload::Error(e)).await;
                }
            }
            FrontendRequest::ModelSet {
                model,
                effort,
                effort_toggled,
                speed,
            } => {
                let result = self
                    .apply_model_selection(ModelSelection {
                        model,
                        effort,
                        effort_toggled,
                        speed,
                        effective_from: EffectiveFrom::NextRequest,
                    })
                    .await;
                reply(io, req_id, ResponsePayload::ModelResult(result)).await;
            }
            payload if reads_state_during_run(&payload) => {
                if let Err(e) = self.dispatch_query(io, req_id, payload).await {
                    reply(io, req_id, ResponsePayload::Error(e)).await;
                }
            }
            // A request that must wait for the turn is answered, not dropped:
            // the client blocks on a response for every request it sends, so
            // dropping one leaves it waiting with no way to tell a slow server
            // from a refused command.
            _ => {
                reply(
                    io,
                    req_id,
                    ResponsePayload::Error(ProtocolError::new(
                        ErrorCategory::InvalidRequest,
                        "this request waits for the running turn to finish",
                        true,
                    )),
                )
                .await;
            }
        }
    }

    /// Handle a session/* JSON-RPC notification received mid-run or
    /// between runs. session/cancel aborts the in-flight run; session/inject
    /// enqueues an identified user message for mid-turn injection; the drive
    /// loop drains it at the next turn boundary. session/queue_remove drops
    /// only the identified item, so equal message text is unambiguous. All
    /// three are fire-and-forget; effects appear in the run outcome and
    /// transcript. Legacy text-only injection remains accepted.
    pub(super) fn handle_session_notification(&self, notif: &AcpNotification) {
        match notif.method.as_str() {
            "session/cancel" => self.runner.abort(),
            "session/inject" => {
                let input = notif.params.as_ref().and_then(|params| {
                    params
                        .get("input")
                        .and_then(|value| serde_json::from_value(value.clone()).ok())
                        .or_else(|| {
                            params
                                .get("text")
                                .and_then(|value| value.as_str())
                                .filter(|text| !text.is_empty())
                                .map(QueuedInput::new)
                        })
                });
                if let Some(input) = input {
                    self.runner.enqueue_input(input);
                }
            }
            "session/queue_remove" => {
                let Some(params) = notif.params.as_ref() else {
                    return;
                };
                if let Some(id) = params
                    .get("id")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                {
                    self.runner.remove_input(id);
                } else if let Some(text) = params.get("text").and_then(|value| value.as_str()) {
                    self.runner.remove_input_by_text(text);
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
            "session/cancel_child_turn" => {
                // Per-turn abort: the viewed-child Esc path. Cancels the
                // child's in-flight model call without killing the run; the
                // drive loop appends an interrupt marker + starts the next
                // turn. Fire-and-forget; a missing runtime or a child
                // between turns is a no-op.
                let Some(child_sid) = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("childSid"))
                    .and_then(|v| v.as_str())
                else {
                    return;
                };
                self.runner.cancel_child(child_sid);
            }
            "session/kill_child" => {
                let Some(child_sid) = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("childSid"))
                    .and_then(|v| v.as_str())
                else {
                    return;
                };
                self.runner.kill_child(child_sid);
            }
            "session/kill_all" => {
                // Kill every live background child: the fleet kill-all path.
                // Each killed child's completion publishes and drops the
                // strip row; a no-op when no runtime or no live children.
                self.runner.kill_all_children();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;

/// Whether a request only reads state, or writes state a running turn does not
/// read, so answering it mid-run cannot race the turn.
///
/// A request that mutates the conversation or the workspace is deliberately
/// absent: those wait for the turn and are answered with an error rather than
/// applied underneath it. The permission toggle is present: like the mode
/// cycle, it writes state the drive loop reads at its next decision, so it
/// takes effect without racing the turn.
fn reads_state_during_run(payload: &FrontendRequest) -> bool {
    matches!(
        payload,
        FrontendRequest::Status
            | FrontendRequest::SandboxStatus
            | FrontendRequest::Metrics
            | FrontendRequest::Context
            | FrontendRequest::Trajectory
            | FrontendRequest::Agents
            | FrontendRequest::Worktree
            | FrontendRequest::ToolList
            | FrontendRequest::Hooks
            | FrontendRequest::Skills
            | FrontendRequest::SkillBody { .. }
            | FrontendRequest::MemoryList
            | FrontendRequest::MemoryShow { .. }
            | FrontendRequest::MemoryToggleState
            | FrontendRequest::ModelInfo
            | FrontendRequest::ChildTranscript { .. }
            | FrontendRequest::RenameSession { .. }
            | FrontendRequest::PermissionMode
            | FrontendRequest::PermissionRules
            | FrontendRequest::PermissionAskBeforeGit { .. }
    )
}

/// Send one response frame. An encode failure is survivable: the serve loop
/// stays alive rather than panicking the task, matching the between-run
/// dispatch path's graceful error return.
async fn reply(io: &mut FrameCarrier, req_id: RequestId, payload: ResponsePayload) {
    let frame = ServerFrame::Response(ResponseEnvelope::new(req_id, payload));
    if let Ok(encoded) = encode(&frame) {
        drop(io.send_frame(encoded).await);
    }
}
