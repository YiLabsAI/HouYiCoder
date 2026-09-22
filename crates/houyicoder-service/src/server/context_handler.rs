//! Context breakdown and compaction handlers for the server dispatch.

use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};

use super::{Server, frame_carrier::FrameCarrier};
use crate::protocol_adapter as pa;

impl Server {
    /// Reply with the assembled context breakdown. When no turn has run yet
    /// the prospective measurement stands in, so the pane is never empty on a
    /// fresh session.
    pub(super) async fn handle_context(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let snap = self.runner.status_snapshot();
        let measurement = self
            .runner
            .last_measurement()
            .unwrap_or_else(|| self.runner.prospective_measurement());
        let mut bd = measurement.breakdown(&snap.model, snap.context_window);
        bd.compact_summary = self.runner.compact_summary(self.session).await;
        let summary_tokens = self.runner.compact_summary_tokens(self.session).await;
        if summary_tokens > 0 {
            let insert_at = bd
                .categories
                .iter()
                .position(|c| c.label == "Free space")
                .unwrap_or(bd.categories.len());
            bd.categories.insert(
                insert_at,
                houyicoder_core::agent::CategoryBreakdown {
                    label: "Compact buffer".into(),
                    color_hint: 61,
                    tokens: summary_tokens,
                    is_deferred: false,
                    is_reserved: false,
                },
            );
        }
        let prefix: u32 = measurement
            .section(houyicoder_core::agent::SectionKind::SystemPrompt)
            .map(|s| s.tokens)
            .unwrap_or(0)
            + measurement
                .section(houyicoder_core::agent::SectionKind::Tools)
                .map(|s| s.tokens)
                .unwrap_or(0);
        bd.cache_prefix_tokens = Some(prefix);
        let usage = &snap.cumulative_usage;
        if usage.input_tokens > 0 {
            bd.cache_hit_rate =
                Some(usage.cache_read_input_tokens as f64 / usage.input_tokens as f64);
        }
        let context = pa::map_context_breakdown(&bd);
        self.send_response(io, req_id, ResponsePayload::Context(context))
            .await
    }

    /// Fold older events into a summary and reply with the outcome. The runner
    /// fires the compaction hooks and marker extraction internally, so the
    /// manual path and the automatic overflow path share one sequence. The
    /// assembled context picks up the manifest on the next turn: compaction
    /// does not shrink the in-flight window immediately.
    pub(super) async fn handle_compact(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        match self.runner.compact(self.session).await {
            Ok(outcome) => {
                let reply = pa::compaction::map_compact_reply(&outcome);
                self.send_response(io, req_id, ResponsePayload::Compact(reply))
                    .await
            }
            Err(e) => {
                self.send_response(
                    io,
                    req_id,
                    ResponsePayload::Error(ProtocolError::new(
                        ErrorCategory::InvalidRequest,
                        e.to_string(),
                        false,
                    )),
                )
                .await
            }
        }
    }
}
