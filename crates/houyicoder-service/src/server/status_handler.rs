//! Status snapshot assembly, session rename, and title derivation.

use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::SessionId;

use super::{Server, frame_carrier::FrameCarrier};
use crate::protocol_adapter as pa;

impl Server {
    pub(super) async fn handle_status(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let status = self.build_status_snapshot();
        self.send_response(io, req_id, ResponsePayload::Status(status))
            .await
    }

    pub(super) async fn handle_rename_session(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        session_id: SessionId,
        name: String,
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
        let Some(store) = self.descriptor_store.as_ref() else {
            return self
                .send_response(
                    io,
                    req_id,
                    ResponsePayload::Error(ProtocolError::new(
                        ErrorCategory::Internal,
                        "rename: no session descriptor store wired (stub mode)",
                        false,
                    )),
                )
                .await;
        };
        // An empty name clears back to Auto so the display reverts to the
        // first-prompt title; a non-empty name marks User so a later
        // auto-derivation does not clobber it. Applied through
        // update_descriptor so a model switch landing at the same moment does
        // not write back the pre-rename name.
        let trimmed = name.trim().to_string();
        let outcome = store.update_descriptor(self.session, &mut |descriptor| {
            if trimmed.is_empty() {
                descriptor.name = None;
                descriptor.name_source = houyicoder_context::NameSource::Auto;
            } else {
                descriptor.name = Some(trimmed.clone());
                descriptor.name_source = houyicoder_context::NameSource::User;
            }
        });
        let detail = match outcome {
            Ok(houyicoder_context::DescriptorUpdate::Written) => None,
            Ok(houyicoder_context::DescriptorUpdate::Absent) => {
                Some("rename: no session descriptor to rename".to_string())
            }
            Err(e) => Some(format!("rename: write failed: {e}")),
        };
        if let Some(detail) = detail {
            return self
                .send_response(
                    io,
                    req_id,
                    ResponsePayload::Error(ProtocolError::new(
                        ErrorCategory::Internal,
                        detail,
                        false,
                    )),
                )
                .await;
        }
        let status = self.build_status_snapshot();
        self.send_response(io, req_id, ResponsePayload::Status(status))
            .await
    }

    /// Build the current protocol status, deriving an unnamed session's display
    /// name from the first prompt when its descriptor is available.
    pub(super) fn build_status_snapshot(
        &self,
    ) -> houyicoder_protocol::frontend::status::StatusSnapshot {
        let snap = self.runner.status_snapshot();
        let mut snapshot = pa::map_status_snapshot(&snap);
        if let Some(store) = self.descriptor_store.as_ref()
            && let Some(mut descriptor) = store.read_descriptor(self.session)
        {
            if descriptor
                .name
                .as_ref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
            {
                descriptor.name = first_prompt_title(self.runner.store().as_ref(), self.session);
            }
            snapshot.descriptor = Some(pa::map_session_descriptor(&descriptor));
        }
        snapshot.version = env!("CARGO_PKG_VERSION").to_string();
        snapshot.auth_token_source = super::status_projection::auth_token_source();
        snapshot.base_url = houyicoder_config::resolve_base_url();
        snapshot.setting_sources = super::status_projection::setting_sources_label();
        let (toggles, _settings_warnings) = houyicoder_config::load_toggles();
        snapshot.auto_memory = toggles.auto_memory;
        snapshot.auto_dream = toggles.auto_dream;
        snapshot.by_model =
            super::status_projection::project_by_model(self.runner.by_model_usage());
        snapshot
    }
}

/// Derive a session title from the first user prompt in the log head, so an
/// unnamed session (name_source=Auto) shows a title instead of blank. Reads
/// only the bounded log head, not a full replay, so it stays cheap even for a
/// large resumed session.
pub(super) fn first_prompt_title(
    log: &dyn houyicoder_api::session::SessionLog,
    session: houyicoder_context::SessionId,
) -> Option<String> {
    use houyicoder_context::{SessionEvent, SessionLogEntry};
    let read = log.backend().read_log_range(session, 0, 64_000);
    for (_, line) in &read.lines {
        if let Ok(ev) = serde_json::from_str::<SessionLogEntry>(line)
            && let SessionEvent::UserInput { text } = &ev.event
        {
            return Some(compact_session_title(text));
        }
    }
    None
}

/// Convert a prompt into a compact session title: lowercase words joined by
/// dashes, truncated with an ellipsis past the display budget.
pub(super) fn compact_session_title(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_dash = true;
    for c in text.chars().take(50) {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.chars().count() > 40 {
        let mut truncated: String = out.chars().take(39).collect();
        while truncated.ends_with('-') {
            truncated.pop();
        }
        truncated.push('\u{2026}');
        truncated
    } else {
        out
    }
}

#[cfg(test)]
#[path = "status_handler_tests.rs"]
mod tests;
