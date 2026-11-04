//! The notification-consumption turn: same drive cycle as a user message.

use std::collections::HashSet;
use std::sync::Arc;

use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::RunOutcome;
use houyicoder_core::agent::multi_agent::bus_types::permission_request_topic;
use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::SessionId;

use super::Server;
use super::frame_carrier::FrameCarrier;
use super::{OnClose, map_run_error, map_run_result};

impl Server {
    /// Run the notification-consumption turn an idle client asked for.
    pub(super) async fn dispatch_notification_drain(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        session_id: SessionId,
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
        let runner = Arc::clone(&self.runner);
        let run_fut = runner.run_notification_turn(self.session);
        let mut perm_rx = self
            .bus
            .as_ref()
            .map(|b| b.subscribe(permission_request_topic()));
        let mut result = self
            .drive_while(
                io,
                run_fut,
                "client closed mid-drain",
                perm_rx.as_mut(),
                OnClose::Abandon,
            )
            .await?;
        self.flush_events(io).await?;
        loop {
            match result {
                Ok(run) => match run.outcome {
                    RunOutcome::Interruption(approvals) => {
                        let decisions = self.collect_interruption_decisions(io, approvals).await?;
                        result = self.drive_resume_step(io, &decisions).await?;
                        self.flush_events(io).await?;
                    }
                    _ => {
                        let payload = ResponsePayload::RunOk(map_run_result(&run));
                        return self.send_response(io, req_id, payload).await;
                    }
                },
                Err(e) => {
                    let payload = ResponsePayload::RunErr(map_run_error(&e));
                    return self.send_response(io, req_id, payload).await;
                }
            }
        }
    }
}
