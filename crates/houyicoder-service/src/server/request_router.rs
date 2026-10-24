//! Request dispatch router for the server.

use houyicoder_protocol::envelope::{RequestEnvelope, RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::model::EffectiveFrom;
use houyicoder_protocol::frontend::{FrontendRequest, SessionId};

use super::model_apply::ModelSelection;
use super::{Server, frame_carrier::FrameCarrier};
use crate::protocol_adapter as pa;
use houyicoder_api::session::TrajectoryPages;
use houyicoder_api::session::TrajectoryPosition;
use houyicoder_context::ContextError;
use houyicoder_context::SessionEvent;
use houyicoder_context::SessionLogEntry;
use houyicoder_core::snapshot::UndoOutcome;
use houyicoder_protocol::frontend::model::SpeedMode;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::trajectory::TrajectoryResponse;
use houyicoder_protocol::llm::EffortLevel;
use pa::hooks::declared_events;
use pa::hooks::hook_entries;
use pa::permission_mode_to_wire;
use pa::redundancy::map_redundant_entries;
use pa::skills::skill_entries;
use std::collections::HashSet;

impl Server {
    /// True when the protocol session id names this server's session. The
    /// protocol id is a display string; the engine session is ULID-backed, so
    /// the match is on the display form.
    pub(super) fn session_matches(&self, wire_id: &SessionId) -> bool {
        wire_id.0 == self.session.to_string()
    }

    /// Every durable record of this session's current epoch, in append order.
    ///
    /// The records are read page by page and released into the result, so a
    /// reader that has to hold the whole history pays for it once rather than
    /// keeping a second copy in the log state. Callers that can consume the
    /// history incrementally read the pages directly.
    fn durable_history(&self) -> Result<Vec<SessionLogEntry>, ContextError> {
        let store = self.runner.store();
        let pages = TrajectoryPages::new(store.as_ref(), self.session, TrajectoryPosition::start());
        let mut events = Vec::new();
        for page in pages {
            events.extend(page?.events);
        }
        Ok(events)
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
            FrontendRequest::SkillBody { name } => self.dispatch_skill_body(io, req_id, name).await,
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
            FrontendRequest::PermissionMode
            | FrontendRequest::PermissionRules
            | FrontendRequest::PermissionCycleMode
            | FrontendRequest::PermissionAddRule { .. }
            | FrontendRequest::PermissionRemoveRule { .. }
            | FrontendRequest::PermissionAddWorkingDir { .. }
            | FrontendRequest::PermissionRemoveWorkingDir { .. }
            | FrontendRequest::PermissionAskBeforeGit { .. } => {
                self.dispatch_permission(io, req_id, payload).await
            }
            _ => self.send_response(io, req_id, ResponsePayload::Ack).await,
        }
    }

    /// Route a permission-management request: the mode, the durable rule
    /// set, the working-dir widening, and the git ask toggle. Kept apart
    /// from the read-only queries so each router stays a readable dispatch
    /// table.
    async fn dispatch_permission(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        payload: FrontendRequest,
    ) -> Result<(), ProtocolError> {
        match payload {
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
        content: Vec<ContentBlock>,
        disabled_skills: HashSet<String>,
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
        let mut hooks = hook_entries(self.runner.hooks_list());
        hooks.extend(declared_events());
        hooks.sort_by(|a, b| a.name.cmp(&b.name));
        self.send_response(io, req_id, ResponsePayload::Hooks(hooks))
            .await
    }

    async fn dispatch_skills(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let skills = skill_entries(self.runner.skills_snapshot());
        self.send_response(io, req_id, ResponsePayload::Skills(skills))
            .await
    }

    /// The body one skill would inject, for the /skills detail view. A
    /// registry that cannot resolve the name, or a body file that cannot be
    /// read, replies None: the pane renders the detail with an unavailable
    /// note rather than dropping the open view.
    async fn dispatch_skill_body(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        name: String,
    ) -> Result<(), ProtocolError> {
        let body = match self.runner.skill_registry() {
            Some(registry) => match registry.prepare_body(&name, None, None) {
                Ok(body) => Some(body),
                Err(err) => {
                    tracing::warn!(skill = %name, error = %err, "skill body unavailable");
                    None
                }
            },
            None => {
                tracing::warn!(skill = %name, "no skill registry to read the body from");
                None
            }
        };
        self.send_response(io, req_id, ResponsePayload::SkillBody(body))
            .await
    }

    async fn dispatch_undo(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let desc = match self.runner.undo_last() {
            UndoOutcome::Restored(entry) => Some(entry.description()),
            UndoOutcome::Empty => None,
            UndoOutcome::Failed(msg) => Some(format!("restore failed: {msg}")),
        };
        self.send_response(io, req_id, ResponsePayload::UndoResult(desc))
            .await
    }

    async fn dispatch_model_set(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        model: Option<String>,
        effort: Option<EffortLevel>,
        effort_toggled: bool,
        speed: Option<SpeedMode>,
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
        let events = match self.durable_history() {
            Ok(events) => events,
            Err(e) => {
                return self
                    .send_response(
                        io,
                        req_id,
                        ResponsePayload::Error(ProtocolError::new(
                            ErrorCategory::Unavailable,
                            format!("trajectory read failed: {e}"),
                            false,
                        )),
                    )
                    .await;
            }
        };
        let entries = pa::build_trajectory_entries(&events);
        let redundant = map_redundant_entries(&self.runner.redundancy_snapshot());
        let unknown_count = events
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::Unknown))
            .count() as u32;
        let trajectory = TrajectoryResponse {
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
        self.runner.before_clear(self.session).await;
        self.runner.reset_usage();
        self.runner.reset_measurement();
        self.runner.reset_trajectory(self.session).await;
        self.runner.clear_input_queue();
        self.runner.clear_notifications();
        self.event_sequencer.reset_sequencer();
        self.send_response(io, req_id, ResponsePayload::Ack).await
    }

    /// Route a request received while a turn is running.
    ///
    /// Payloads that read state or take effect at the next boundary are
    /// answered directly; payloads that mutate the conversation or workspace
    /// are answered with an error so the client never blocks waiting for a
    /// dropped request.
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
                    Err(e) => ResponsePayload::Error(ProtocolError::new(
                        ErrorCategory::InvalidRequest,
                        format!("permission mode cycle refused: {e}"),
                        true,
                    )),
                };
                drop(self.send_response(io, req_id, payload).await);
            }
            FrontendRequest::RunCancel { session_id, .. } => {
                if let Err(e) = self.dispatch_run_cancel(io, req_id, session_id).await {
                    drop(
                        self.send_response(io, req_id, ResponsePayload::Error(e))
                            .await,
                    );
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
                drop(
                    self.send_response(io, req_id, ResponsePayload::ModelResult(result))
                        .await,
                );
            }
            payload if reads_state_during_run(&payload) => {
                if let Err(e) = self.dispatch_query(io, req_id, payload).await {
                    drop(
                        self.send_response(io, req_id, ResponsePayload::Error(e))
                            .await,
                    );
                }
            }
            _ => {
                drop(
                    self.send_response(
                        io,
                        req_id,
                        ResponsePayload::Error(ProtocolError::new(
                            ErrorCategory::InvalidRequest,
                            "this request waits for the running turn to finish",
                            true,
                        )),
                    )
                    .await,
                );
            }
        }
    }
}

/// Whether a request only reads state, or writes state a running turn does not
/// read, so answering it mid-run cannot race the turn.
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

#[cfg(test)]
mod tests;
