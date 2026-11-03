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
        // time rather than clone and scan the growing event log state. Both
        // background accounts belong to the session total: a reader comparing
        // the figures against the rows that split them must find the split
        // already inside.
        let head = self.runner.store().trajectory_head(self.session);
        let background = head.summary.usage;
        snapshot.cumulative_usage = snapshot
            .cumulative_usage
            .saturating_add(&background.subagent.to_usage())
            .saturating_add(&background.recap.to_usage());
        snapshot.session_audit = pa::map_session_audit(&head.summary);
        // Execution results are durable too: the fold counts every call and
        // judges every result from the log, so the row reports the session
        // rather than what this process ran. The runner's own counters start
        // at zero after a reopen. Delegated calls are inside these figures,
        // the same ones the trajectory header reports.
        let calls = u32::try_from(head.summary.usage.tool_calls).unwrap_or(u32::MAX);
        let failures = u32::try_from(head.summary.usage.failures)
            .unwrap_or(u32::MAX)
            .min(calls);
        // Successes are counted from the results, not derived by subtracting
        // from the calls: a call whose turn ended before its result arrived
        // has no known outcome, and the subtraction would report one.
        let successes = u32::try_from(head.summary.usage.successes).unwrap_or(u32::MAX);
        snapshot.tool_calls = calls;
        snapshot.tool_errors = failures;
        snapshot.tool_success = successes;
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
        // The four settings-file switches read from the live session, not the
        // file: status is polled every second, and a value the user just
        // changed must be the one this reply carries. Reading the file here
        // would also open it twice per second and report a stale window for
        // the second between the write and the next poll.
        let gates = self.runner.memory_gate_state();
        snapshot.auto_memory = gates.auto_memory;
        snapshot.auto_dream = gates.auto_dream;
        let recap = self
            .recap
            .as_ref()
            .map(|recap| recap.settings())
            .unwrap_or_default();
        snapshot.recap_enabled = recap.enabled;
        snapshot.recap_idle_secs = recap.idle_trigger_secs;
        snapshot.by_model = build_model_usage_view(self.runner.by_model_usage());
        snapshot
    }
}

#[cfg(test)]
mod tests;
