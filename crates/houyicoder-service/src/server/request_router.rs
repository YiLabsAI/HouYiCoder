//! Request dispatch router for the server.

use houyicoder_protocol::envelope::{RequestEnvelope, RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::model::EffectiveFrom;
use houyicoder_protocol::frontend::{FrontendRequest, SessionId};

use super::model_apply::ModelSelection;
use super::{Server, frame_carrier::FrameCarrier};
use crate::protocol_adapter as pa;

impl Server {
    /// True when the protocol session id names this server's session. The
    /// protocol id is a display string; the engine session is ULID-backed, so
    /// the match is on the display form.
    pub(super) fn session_matches(&self, wire_id: &SessionId) -> bool {
        wire_id.0 == self.session.to_string()
    }

    pub(super) async fn dispatch(
        &mut self,
        io: &mut FrameCarrier,
        req: RequestEnvelope,
    ) -> Result<(), ProtocolError> {
        let req_id = req.req_id;
        match req.payload {
            FrontendRequest::MessageSend {
                session_id,
                content,
                disabled_skills,
            } => {
                self.dispatch_message_send(io, req_id, session_id, content, disabled_skills)
                    .await
            }
            FrontendRequest::RunCancel { session_id, .. } => {
                self.dispatch_run_cancel(io, req_id, session_id).await
            }
            FrontendRequest::SessionReset { session_id } => {
                self.dispatch_session_reset(io, req_id, session_id).await
            }
            FrontendRequest::DebugSet { level } => {
                self.send_response(io, req_id, self.debug_response(level))
                    .await
            }
            payload => self.dispatch_query(io, req_id, payload).await,
        }
    }

    /// Route a read-only or state-scoped request: panes, catalog listings,
    /// model selection, memory, and permission management. Separated from the
    /// run verbs so each router stays a readable dispatch table.
    async fn dispatch_query(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        payload: FrontendRequest,
    ) -> Result<(), ProtocolError> {
        match payload {
            FrontendRequest::Status => self.handle_status(io, req_id).await,
            FrontendRequest::RenameSession { session_id, name } => {
                self.handle_rename_session(io, req_id, session_id, name)
                    .await
            }
            FrontendRequest::ToolList => self.handle_tool_list(io, req_id).await,
            FrontendRequest::Agents => self.handle_agents(io, req_id).await,
            FrontendRequest::ChildTranscript { child_sid } => {
                let frames = self.child_transcript_frames(&child_sid).await;
                self.send_response(
                    io,
                    req_id,
                    ResponsePayload::ChildTranscript { child_sid, frames },
                )
                .await
            }
            FrontendRequest::Hooks => self.dispatch_hooks(io, req_id).await,
            FrontendRequest::Skills => self.dispatch_skills(io, req_id).await,
            FrontendRequest::Undo => self.dispatch_undo(io, req_id).await,
            FrontendRequest::ModelInfo => self.handle_model_info(io, req_id).await,
            FrontendRequest::ModelSet {
                model,
                effort,
                effort_toggled,
                speed,
            } => {
                self.dispatch_model_set(io, req_id, model, effort, effort_toggled, speed)
                    .await
            }
            FrontendRequest::Trajectory => self.dispatch_trajectory(io, req_id).await,
            FrontendRequest::Context => self.handle_context(io, req_id).await,
            FrontendRequest::Compact => self.handle_compact(io, req_id).await,
            FrontendRequest::MemoryList => self.handle_memory_list(io, req_id).await,
            FrontendRequest::MemoryShow { key } => self.handle_memory_show(io, req_id, key).await,
            FrontendRequest::MemoryForget { key, scope } => {
                self.handle_memory_forget(io, req_id, key, scope).await
            }
            FrontendRequest::MemoryToggleState => self.handle_memory_toggle_state(io, req_id).await,
            FrontendRequest::MemoryToggle { which } => {
                self.handle_memory_toggle(io, req_id, which).await
            }
            FrontendRequest::PermissionMode => self.handle_permission_mode(io, req_id).await,
            FrontendRequest::PermissionRules => self.handle_permission_rules(io, req_id).await,
            FrontendRequest::PermissionCycleMode => {
                self.handle_permission_cycle_mode(io, req_id).await
            }
            FrontendRequest::PermissionAddRule { rule } => {
                self.handle_permission_add_rule(io, req_id, rule).await
            }
            FrontendRequest::PermissionRemoveRule { index } => {
                self.handle_permission_remove_rule(io, req_id, index).await
            }
            FrontendRequest::PermissionAddWorkingDir { path } => {
                self.handle_permission_add_working_dir(io, req_id, path)
                    .await
            }
            FrontendRequest::PermissionRemoveWorkingDir { path } => {
                self.handle_permission_remove_working_dir(io, req_id, path)
                    .await
            }
            FrontendRequest::PermissionAskBeforeGit { enabled } => {
                self.send_response(io, req_id, self.ask_before_git_response(enabled))
                    .await
            }
            _ => self.send_response(io, req_id, ResponsePayload::Ack).await,
        }
    }

    async fn dispatch_message_send(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        session_id: SessionId,
        content: Vec<houyicoder_protocol::frontend::run::ContentBlock>,
        disabled_skills: std::collections::HashSet<String>,
    ) -> Result<(), ProtocolError> {
        if !self.session_matches(&session_id) {
            return self
                .send_response(
                    io,
                    req_id,
                    ResponsePayload::Error(ProtocolError::new(
                        ErrorCategory::InvalidRequest,
                        format!("session id mismatch: got {session_id}"),
                        false,
                    )),
                )
                .await;
        }
        if let Some(reg) = self.runner.skill_registry() {
            reg.set_session_disabled(disabled_skills);
        }
        self.handle_message_send(io, req_id, content).await
    }

    async fn dispatch_run_cancel(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        session_id: SessionId,
    ) -> Result<(), ProtocolError> {
        if !self.session_matches(&session_id) {
            return self
                .send_response(
                    io,
                    req_id,
                    ResponsePayload::Error(ProtocolError::new(
                        ErrorCategory::InvalidRequest,
                        format!("session id mismatch: got {session_id}"),
                        false,
                    )),
                )
                .await;
        }
        self.runner.abort();
        self.send_response(io, req_id, ResponsePayload::Ack).await
    }

    async fn dispatch_hooks(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let mut hooks = pa::hooks::hook_entries(self.runner.hooks_list());
        hooks.extend(pa::hooks::declared_events());
        hooks.sort_by(|a, b| a.name.cmp(&b.name));
        self.send_response(io, req_id, ResponsePayload::Hooks(hooks))
            .await
    }

    async fn dispatch_skills(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let skills = pa::skills::skill_entries(self.runner.skills_snapshot());
        self.send_response(io, req_id, ResponsePayload::Skills(skills))
            .await
    }

    async fn dispatch_undo(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let desc = match self.runner.undo_last() {
            houyicoder_core::snapshot::UndoOutcome::Restored(entry) => Some(entry.description()),
            houyicoder_core::snapshot::UndoOutcome::Empty => None,
            houyicoder_core::snapshot::UndoOutcome::Failed(msg) => {
                Some(format!("restore failed: {msg}"))
            }
        };
        self.send_response(io, req_id, ResponsePayload::UndoResult(desc))
            .await
    }

    async fn dispatch_model_set(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        model: Option<String>,
        effort: Option<houyicoder_protocol::llm::EffortLevel>,
        effort_toggled: bool,
        speed: Option<houyicoder_protocol::frontend::model::SpeedMode>,
    ) -> Result<(), ProtocolError> {
        let result = self
            .apply_model_selection(ModelSelection {
                model,
                effort,
                effort_toggled,
                speed,
                effective_from: EffectiveFrom::Immediate,
            })
            .await;
        self.send_response(io, req_id, ResponsePayload::ModelResult(result))
            .await
    }

    async fn dispatch_trajectory(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let events = self.runner.store().trajectory_snapshot(self.session);
        let entries = pa::build_trajectory_entries(&events);
        let redundant = pa::redundancy::map_redundant_entries(&self.runner.redundancy_snapshot());
        let unknown_count = events
            .iter()
            .filter(|e| matches!(e.event, houyicoder_context::SessionEvent::Unknown))
            .count() as u32;
        let trajectory = houyicoder_protocol::frontend::trajectory::TrajectoryResponse {
            entries,
            redundant,
            unknown_count,
        };
        self.send_response(io, req_id, ResponsePayload::Trajectory(trajectory))
            .await
    }

    async fn dispatch_session_reset(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        session_id: SessionId,
    ) -> Result<(), ProtocolError> {
        if session_id.0 != self.session.to_string() {
            return self
                .send_response(
                    io,
                    req_id,
                    ResponsePayload::Error(ProtocolError::new(
                        ErrorCategory::InvalidRequest,
                        "session id mismatch",
                        false,
                    )),
                )
                .await;
        }
        if let Err(e) = self.runner.before_clear(self.session).await {
            tracing::warn!("before-clear extraction failed: {e}");
        }
        self.runner.reset_usage();
        self.runner.reset_trajectory(self.session).await;
        self.runner.clear_input_queue();
        self.runner.clear_notifications();
        self.event_sequencer.reset_projection();
        self.send_response(io, req_id, ResponsePayload::Ack).await
    }
}

#[cfg(test)]
#[path = "trajectory_handler_tests.rs"]
mod trajectory_handler_tests;

#[cfg(test)]
#[path = "rename_session_tests.rs"]
mod rename_session_tests;

#[cfg(test)]
#[path = "memory_tests.rs"]
mod memory_tests;

#[cfg(test)]
#[path = "context_dispatch_tests.rs"]
mod context_dispatch_tests;

#[cfg(test)]
#[path = "model_set_tests.rs"]
mod model_set_tests;

#[cfg(test)]
#[path = "request_rejection_tests.rs"]
mod request_rejection_tests;

#[cfg(test)]
#[path = "dispatch_response_tests.rs"]
mod dispatch_response_tests;
