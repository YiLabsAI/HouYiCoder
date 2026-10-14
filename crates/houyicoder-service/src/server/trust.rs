//! Startup workspace-trust handshake.
//! Untrusted projects must receive a client verdict before the frame loop.

use houyicoder_api::trust::TrustState;
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponsePayload, ServerFrame, ServerRequestEnvelope, ServerRequestPayload,
};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::trust::TrustPrompt;

use super::Server;
use super::frame_carrier::FrameCarrier;
use houyicoder_protocol::acp_wire::AcpNotification;
use std::path::Path;

/// Resolve project trust from user-owned settings. Ancestor trust applies to
/// descendants; a session without a project is trusted.
pub(crate) fn compute_trust_state(settings_path: &Path, project_path: Option<&Path>) -> TrustState {
    let Some(path) = project_path else {
        return TrustState::Trusted;
    };
    if houyicoder_config::is_path_trusted(settings_path, path) {
        TrustState::Acknowledged
    } else {
        TrustState::Untrusted
    }
}

impl Server {
    /// Resolve startup trust before serving frames. Acceptance persists the
    /// project boundary; rejection ends the session.
    pub(crate) async fn ensure_trust(
        &mut self,
        io: &mut FrameCarrier,
    ) -> Result<TrustState, ProtocolError> {
        // Clone the path + settings path into owned locals so no shared
        // borrow of self spans the mutable send_typed / mint_req_id calls.
        let project_path = match self.project_path.as_deref() {
            Some(p) => p.to_path_buf(),
            None => return Ok(TrustState::Trusted),
        };
        let settings_path = self.settings_path.clone();
        let state = compute_trust_state(&settings_path, Some(&project_path));
        if state != TrustState::Untrusted {
            return Ok(state);
        }
        let ask_id = self.mint_req_id();
        let prompt = ServerRequestEnvelope::new(
            ask_id,
            ServerRequestPayload::TrustPrompt(TrustPrompt {
                project_path: project_path.to_string_lossy().into_owned(),
                risks: Vec::new(),
            }),
        );
        self.send_typed(io, &ServerFrame::Request(prompt)).await?;
        // Ignore unrelated frames while preserving cancellation as decline.
        let resp = loop {
            let frame = match io.next_frame().await {
                Some(f) => f,
                None => {
                    return Err(ProtocolError::new(
                        ErrorCategory::Unavailable,
                        "client closed mid-trust-prompt",
                        false,
                    ));
                }
            };
            if let Ok(notif) = serde_json::from_str::<AcpNotification>(&frame) {
                let is_cancel = notif.method == "session/cancel";
                self.handle_session_notification(&notif);
                if is_cancel {
                    return Err(ProtocolError::new(
                        ErrorCategory::Unavailable,
                        "user declined trust (cancel)",
                        false,
                    ));
                }
                continue;
            }
            if let Ok(ClientFrame::Response(r)) = serde_json::from_str::<ClientFrame>(&frame)
                && r.req_id == ask_id
            {
                break r;
            }
            tracing::warn!(
                "trust-wait: dropped a non-matching frame while waiting for the trust response (first 80 chars): {}",
                frame.chars().take(80).collect::<String>()
            );
        };
        let accept = match resp.payload {
            ClientResponsePayload::TrustAccept(a) => a,
            _ => {
                return Err(ProtocolError::new(
                    ErrorCategory::InvalidFrame,
                    "expected a trust_accept reverse response",
                    false,
                ));
            }
        };
        if !accept.accepted {
            return Err(ProtocolError::new(
                ErrorCategory::Unavailable,
                "user declined to trust the project",
                false,
            ));
        }
        // Persistence failure keeps this session trusted but forces a later recheck.
        if let Err(e) = houyicoder_config::persist_project_trust(&settings_path, &project_path) {
            tracing::warn!("trust persist failed (re-asks next launch): {e:?}");
        }
        Ok(TrustState::Acknowledged)
    }
}

#[cfg(test)]
mod tests;
