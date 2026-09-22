//! Handlers for permission requests in the server dispatch.

use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::permission::PermissionRule;

use super::{Server, frame_carrier::FrameCarrier};
use crate::protocol_adapter as pa;

impl Server {
    /// Project the gate's durable rule set for /rules replies and the
    /// add/remove acks that ship the updated set. Builtin rules ship with the
    /// binary and session rules are transient in-memory consent, so only
    /// writable-scope rules are listed here.
    pub(super) fn permission_rules(&self) -> Vec<PermissionRule> {
        self.gate
            .rules()
            .iter()
            .filter(|r| r.scope.is_writable())
            .map(pa::permission_rule_to_wire)
            .collect()
    }

    /// Project the sandbox session's runtime working dirs for the Workspace
    /// tab and the add/remove acks. Empty when no session is attached.
    pub(super) fn working_directories(&self) -> Vec<String> {
        self.sandbox_session
            .as_ref()
            .map(|s| s.working_dirs())
            .unwrap_or_default()
    }

    pub(super) async fn handle_permission_mode(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let mode = pa::permission_mode_to_wire(self.gate.current());
        self.send_response(io, req_id, ResponsePayload::PermissionMode(mode))
            .await
    }

    pub(super) async fn handle_permission_rules(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        self.send_response(
            io,
            req_id,
            ResponsePayload::PermissionRules(self.permission_rules()),
        )
        .await
    }

    pub(super) async fn handle_permission_cycle_mode(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let resp = match self.gate.tab_cycle() {
            Ok(mode) => ResponsePayload::PermissionMode(pa::permission_mode_to_wire(mode)),
            Err(e) => ResponsePayload::Error(ProtocolError::new(
                ErrorCategory::InvalidRequest,
                e.to_string(),
                false,
            )),
        };
        self.send_response(io, req_id, resp).await
    }

    pub(super) async fn handle_permission_add_rule(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        rule: PermissionRule,
    ) -> Result<(), ProtocolError> {
        let resp = match pa::permission_rule_from_wire(&rule) {
            Ok(r) => {
                self.gate.add_rule(r);
                ResponsePayload::PermissionRules(self.permission_rules())
            }
            Err(e) => ResponsePayload::Error(ProtocolError::new(
                ErrorCategory::InvalidRequest,
                e.to_string(),
                false,
            )),
        };
        self.send_response(io, req_id, resp).await
    }

    pub(super) async fn handle_permission_remove_rule(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        index: usize,
    ) -> Result<(), ProtocolError> {
        let resp = if self.gate.remove_rule(index) {
            ResponsePayload::PermissionRules(self.permission_rules())
        } else {
            ResponsePayload::Error(ProtocolError::new(
                ErrorCategory::InvalidRequest,
                "rule index out of range",
                false,
            ))
        };
        self.send_response(io, req_id, resp).await
    }

    pub(super) async fn handle_permission_add_working_dir(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        path: String,
    ) -> Result<(), ProtocolError> {
        let resp = match &self.sandbox_session {
            Some(s) => match s.add_working_dir(&path) {
                Ok(()) => ResponsePayload::PermissionWorkingDirs(self.working_directories()),
                Err(e) => ResponsePayload::Error(ProtocolError::new(
                    ErrorCategory::InvalidRequest,
                    e.to_string(),
                    false,
                )),
            },
            None => ResponsePayload::Error(ProtocolError::new(
                ErrorCategory::InvalidRequest,
                "no sandbox session attached; working dirs need a runtime-mutable fence",
                false,
            )),
        };
        self.send_response(io, req_id, resp).await
    }

    pub(super) async fn handle_permission_remove_working_dir(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        path: String,
    ) -> Result<(), ProtocolError> {
        if let Some(s) = &self.sandbox_session {
            s.remove_working_dir(&path);
        }
        self.send_response(
            io,
            req_id,
            ResponsePayload::PermissionWorkingDirs(self.working_directories()),
        )
        .await
    }
}
