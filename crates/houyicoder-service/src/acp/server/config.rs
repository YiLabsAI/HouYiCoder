//! The session/set_mode and session/set_config_option handlers, split out
//! from the serve loop so the server module stays under the size gate. They
//! are methods on AcpServer; the serve loop in the parent module dispatches
//! to them by method name.

use super::AcpServer;
use crate::acp::adapter::session_config_options;
use crate::acp::adapter::session_modes;
use crate::acp::pump::AcpDuplexPump;
use houyicoder_permission::PermissionMode;
use houyicoder_protocol::acp_wire::{
    AcpErrorCode, AcpRequest, AcpResponse, SessionConfigOptionValue, SetSessionConfigOptionRequest,
    SetSessionModeRequest,
};
use houyicoder_protocol::error::ProtocolError;

impl AcpServer {
    /// Handle session/set_mode: map the wire mode id to a permission mode,
    /// apply it to the gate, and reply with the resulting mode state. An
    /// unknown id is InvalidParams rather than a silent no-op.
    pub(super) async fn handle_set_mode(
        &mut self,
        io: &mut AcpDuplexPump,
        req: &AcpRequest,
    ) -> Result<(), ProtocolError> {
        let params: SetSessionModeRequest = match req.params.as_ref() {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(p) => p,
                Err(e) => {
                    return self
                        .send_response(
                            io,
                            &AcpResponse::err(
                                req.id.clone(),
                                AcpErrorCode::InvalidParams,
                                format!("bad params: {e}"),
                            ),
                        )
                        .await;
                }
            },
            None => {
                return self
                    .send_response(
                        io,
                        &AcpResponse::err(
                            req.id.clone(),
                            AcpErrorCode::InvalidParams,
                            "missing params",
                        ),
                    )
                    .await;
            }
        };
        if params.session_id != self.session.to_string() {
            return self
                .send_response(
                    io,
                    &AcpResponse::err(
                        req.id.clone(),
                        AcpErrorCode::InvalidParams,
                        format!("session id mismatch: got {}", params.session_id),
                    ),
                )
                .await;
        }
        let mode = match params.mode_id.as_str() {
            "manual" => PermissionMode::Manual,
            "auto" => PermissionMode::Auto,
            other => {
                return self
                    .send_response(
                        io,
                        &AcpResponse::err(
                            req.id.clone(),
                            AcpErrorCode::InvalidParams,
                            format!("unknown mode: {other}"),
                        ),
                    )
                    .await;
            }
        };
        self.gate.set_mode(mode, "acp session/set_mode");
        let view = session_modes(self.gate.as_ref());
        self.send_response(
            io,
            &AcpResponse::ok(
                req.id.clone(),
                serde_json::to_value(&view).expect("mode state serialize"),
            ),
        )
        .await
    }

    /// Handle session/set_config_option: the model option switches the
    /// runner's active model when the value is in the catalog; an unknown
    /// value or a different option id is InvalidParams. Replies with the
    /// updated config options so the client sees the new current value.
    pub(super) async fn handle_set_config_option(
        &mut self,
        io: &mut AcpDuplexPump,
        req: &AcpRequest,
    ) -> Result<(), ProtocolError> {
        let params: SetSessionConfigOptionRequest = match req.params.as_ref() {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(p) => p,
                Err(e) => {
                    return self
                        .send_response(
                            io,
                            &AcpResponse::err(
                                req.id.clone(),
                                AcpErrorCode::InvalidParams,
                                format!("bad params: {e}"),
                            ),
                        )
                        .await;
                }
            },
            None => {
                return self
                    .send_response(
                        io,
                        &AcpResponse::err(
                            req.id.clone(),
                            AcpErrorCode::InvalidParams,
                            "missing params",
                        ),
                    )
                    .await;
            }
        };
        if params.session_id != self.session.to_string() {
            return self
                .send_response(
                    io,
                    &AcpResponse::err(
                        req.id.clone(),
                        AcpErrorCode::InvalidParams,
                        format!("session id mismatch: got {}", params.session_id),
                    ),
                )
                .await;
        }
        if params.config_id != "model" {
            return self
                .send_response(
                    io,
                    &AcpResponse::err(
                        req.id.clone(),
                        AcpErrorCode::InvalidParams,
                        format!("unknown config option: {}", params.config_id),
                    ),
                )
                .await;
        }
        let new_model = match &params.value {
            SessionConfigOptionValue::ValueId { value } => value.clone(),
            SessionConfigOptionValue::Boolean { .. } => {
                return self
                    .send_response(
                        io,
                        &AcpResponse::err(
                            req.id.clone(),
                            AcpErrorCode::InvalidParams,
                            "model option expects a value id, not a boolean",
                        ),
                    )
                    .await;
            }
        };
        if !self.model_options.iter().any(|m| m == &new_model) {
            return self
                .send_response(
                    io,
                    &AcpResponse::err(
                        req.id.clone(),
                        AcpErrorCode::InvalidParams,
                        format!("model not in catalog: {new_model}"),
                    ),
                )
                .await;
        }
        self.runner.set_model(new_model);
        let options = session_config_options(&self.runner, &self.model_options);
        self.send_response(
            io,
            &AcpResponse::ok(
                req.id.clone(),
                serde_json::to_value(&options).expect("config options serialize"),
            ),
        )
        .await
    }
}
