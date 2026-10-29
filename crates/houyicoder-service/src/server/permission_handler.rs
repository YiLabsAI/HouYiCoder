//! Handlers for permission requests in the server dispatch.

use houyicoder_permission::Scope;
use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::permission::PermissionRule;
use std::path::Path;

use super::{Server, frame_carrier::FrameCarrier};
use crate::protocol_adapter as pa;

impl Server {
    /// Map the gate's durable rule set for /rules replies and the
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

    /// List the sandbox session's runtime working dirs for the Workspace
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
            Ok(r) => match self.gate.add_rule(r) {
                Ok(()) => ResponsePayload::PermissionRules(self.permission_rules()),
                // The rule did not enter the live set either, so reporting
                // the failure leaves no rule behind that a listing would
                // have to disown.
                Err(e) => ResponsePayload::Error(ProtocolError::new(
                    ErrorCategory::Unavailable,
                    format!("the rule could not be saved: {e}"),
                    false,
                )),
            },
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
                Ok(()) => {
                    // The entry the pane shows is also the grant the fence
                    // hydrates on the next start, so this write is what keeps
                    // the two the same list. It follows the fence: a path the
                    // fence refused must not be stored.
                    match self.gate.add_directory(Path::new(&path), Scope::Local) {
                        Ok(()) => {
                            ResponsePayload::PermissionWorkingDirs(self.working_directories())
                        }
                        // The fence accepted the path for this session; only
                        // the copy that outlives it is missing. The reply still
                        // carries the fence's list, because the pane must show
                        // what is granted right now; the short lifetime is
                        // said out loud as a notice instead of being hidden
                        // behind a list that looks durable.
                        Err(e) => {
                            self.notify_user(format!(
                                "{path} is granted for this session, but it could not be saved for the next start: {e}"
                            ));
                            ResponsePayload::PermissionWorkingDirs(self.working_directories())
                        }
                    }
                }
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
        // The durable grant goes with the runtime one, or the directory would
        // be granted again on the next start and the removal would not hold.
        if let Err(e) = self.gate.remove_directory(Path::new(&path)) {
            // The fence no longer grants it; the saved copy is what failed to
            // go. The reply carries the fence's list because the pane must
            // show what is granted right now, and the notice says the removal
            // may not survive a restart.
            self.notify_user(format!(
                "{path} is no longer granted for this session, but its saved grant could not be dropped, so it may return on the next start: {e}"
            ));
        }
        self.send_response(
            io,
            req_id,
            ResponsePayload::PermissionWorkingDirs(self.working_directories()),
        )
        .await
    }
}
