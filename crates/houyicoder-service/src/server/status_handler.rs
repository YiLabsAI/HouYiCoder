//! Status snapshot assembly and session rename.

use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::SessionId;

use super::status_view::build_model_usage_view;
use super::status_view::setting_sources_label;
use super::{Server, frame_carrier::FrameCarrier};
use crate::protocol_adapter as pa;
use houyicoder_context::DescriptorUpdate;
use houyicoder_context::NameSource;
use houyicoder_context::session_intent::display_title;
use houyicoder_protocol::frontend::status::StatusSnapshot;
use houyicoder_provider::ProviderFacts;

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
        // title its prompts give; a non-empty name marks User so a later
        // auto-derivation does not clobber it. Applied through
        // update_descriptor so a model switch landing at the same moment does
        // not write back the pre-rename name.
        let trimmed = name.trim().to_string();
        let outcome = store.update_descriptor(self.session, &mut |descriptor| {
            if trimmed.is_empty() {
                descriptor.name = None;
                descriptor.name_source = NameSource::Auto;
            } else {
                descriptor.name = Some(trimmed.clone());
                descriptor.name_source = NameSource::User;
            }
        });
        let detail = match outcome {
            Ok(DescriptorUpdate::Written | DescriptorUpdate::Unchanged) => None,
            Ok(DescriptorUpdate::Absent) => {
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

    /// Build the current protocol status, titling an unnamed session from the
    /// prompts its descriptor records and answering from the record alone.
    pub(super) fn build_status_snapshot(&self) -> StatusSnapshot {
        let snap = self.runner.status_snapshot();
        let mut snapshot = pa::map_status_snapshot(&snap);
        // The append owner folds the whole-session figures once. Status is
        // polled every second while idle, so reading them must stay constant
        // time rather than clone and scan the growing event log state.
        let delegated = self
            .runner
            .store()
            .trajectory_head(self.session)
            .summary
            .usage
            .subagent;
        snapshot.cumulative_usage = snapshot
            .cumulative_usage
            .saturating_add(&delegated.to_usage());
        if let Some(store) = self.descriptor_store.as_ref()
            && let Some(mut descriptor) = store.read_descriptor(self.session)
        {
            // One policy names the session everywhere: the name its user gave
            // it, else its newest prompt, else the first one. A record written
            // before prompts were recorded states none until the bounded
            // migration fills it; status answers from the record and never
            // opens a log.
            descriptor.name = display_title(&descriptor).map(|title| title.text.to_string());
            snapshot.descriptor = Some(pa::map_session_descriptor(&descriptor));
        }
        snapshot.version = env!("CARGO_PKG_VERSION").to_string();
        // The facts the provider was built from, carried here rather than
        // re-resolved: a resolver that reads only the environment returns a
        // different endpoint whenever the settings file pins one, so /status
        // would name a host the session never contacts.
        let facts = match self.provider_facts.clone() {
            Some(facts) => facts,
            None => {
                // Only a server assembled without a resolution lands here (the
                // in-process test harness). Say so rather than rendering an
                // environment-derived endpoint that reads as authoritative.
                tracing::warn!(
                    "no provider facts attached; /status reports the environment's view"
                );
                ProviderFacts::unresolved()
            }
        };
        snapshot.base_url = facts.base_url;
        snapshot.auth_token_source = facts.key_origin;
        snapshot.setting_sources = setting_sources_label();
        let (toggles, _settings_warnings) = houyicoder_config::load_toggles();
        snapshot.auto_memory = toggles.auto_memory;
        snapshot.auto_dream = toggles.auto_dream;
        snapshot.by_model = build_model_usage_view(self.runner.by_model_usage());
        snapshot
    }
}

#[cfg(test)]
mod tests;
