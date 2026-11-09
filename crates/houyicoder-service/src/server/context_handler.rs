//! Context breakdown and compaction handlers for the server dispatch.

use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::context::CasUsage;

use super::{Server, frame_carrier::FrameCarrier};
use crate::protocol_adapter as pa;
use crate::protocol_adapter::cas_usage::{map_cas_usage, reports_cas};
use houyicoder_core::agent::CategoryBreakdown;
use houyicoder_core::agent::SectionKind;
use houyicoder_protocol::frontend::context::build_grid;
use pa::compaction::map_compact_reply;

impl Server {
    /// Reply with the assembled context breakdown, measured from the session
    /// as the store holds it. One source serves every session: a resumed one
    /// reports the history it loaded, and a live one reports the turns it has
    /// run. A turn's cached measurement cannot serve here, since the pane also
    /// reports what the session's tools and children cost, which a turn does
    /// not derive.
    pub(super) async fn handle_context(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let snap = self.runner.status_snapshot();
        let res = self.runner.session_context_snapshot(self.session).await;
        let (measurement, compact_summary, summary_tokens) = match res {
            Ok(snapshot) => snapshot,
            Err(e) => {
                return self
                    .send_response(
                        io,
                        req_id,
                        ResponsePayload::Error(ProtocolError::new(
                            ErrorCategory::Internal,
                            e.to_string(),
                            false,
                        )),
                    )
                    .await;
            }
        };
        let mut bd = measurement.breakdown(&snap.model, snap.context_window);
        bd.compact_summary = compact_summary;
        if summary_tokens > 0 {
            // Subtract summary_tokens from Messages to prevent double counting.
            if let Some(msg_cat) = bd.categories.iter_mut().find(|c| c.label == "Messages") {
                msg_cat.tokens = msg_cat.tokens.saturating_sub(summary_tokens);
            }
            let insert_at = bd
                .categories
                .iter()
                .position(|c| c.label == "Free space")
                .unwrap_or(bd.categories.len());
            bd.categories.insert(
                insert_at,
                CategoryBreakdown {
                    label: "Compacted history".into(),
                    color_hint: 109,
                    tokens: summary_tokens,
                    is_deferred: false,
                    is_reserved: false,
                },
            );
        }
        // Add Autocompact buffer reservation when applicable, aligning with
        // proactive auto-compaction safety headroom.
        const AUTOCOMPACT_RESERVE_TOKENS: u32 = 33_000;
        let window = snap.context_window;
        let total_content = bd
            .categories
            .iter()
            .filter(|c| !c.is_reserved && !c.is_deferred && c.label != "Free space")
            .map(|c| c.tokens)
            .sum::<u32>();
        let free_raw = window.saturating_sub(total_content);
        let reserved_tokens = if window >= 100_000 && free_raw > AUTOCOMPACT_RESERVE_TOKENS {
            AUTOCOMPACT_RESERVE_TOKENS
        } else {
            0
        };
        if let Some(free_cat) = bd.categories.iter_mut().find(|c| c.label == "Free space") {
            free_cat.tokens = free_raw.saturating_sub(reserved_tokens);
        }
        if reserved_tokens > 0 {
            bd.categories.push(CategoryBreakdown {
                label: "Autocompact buffer".into(),
                color_hint: 244,
                tokens: reserved_tokens,
                is_deferred: false,
                is_reserved: true,
            });
        }
        // The prefix the request repeats across turns is the whole system
        // prompt, memory blocks included, plus the tool schemas.
        let prefix: u32 = measurement.system_prompt_tokens
            + measurement
                .section(SectionKind::Tools)
                .map(|s| s.tokens)
                .unwrap_or(0);
        bd.cache_prefix_tokens = Some(prefix);
        let usage = &snap.cumulative_usage;
        // The same ratio the status Usage rows render, from one definition.
        bd.cache_hit_rate = usage.cache_hit_rate();
        let cas = self.cas_usage().await;
        // Rebuild grid with updated categories and exact legend row count so
        // the left grid and right legend align without under-run or extra rows.
        let legend_rows_count = 3
            + bd.categories
                .iter()
                .filter(|c| c.tokens > 0 && !c.is_deferred)
                .count()
            + usize::from(bd.cache_prefix_tokens.is_some())
            + usize::from(bd.compact_summary.is_some());
        bd.grid = build_grid(
            &bd.categories,
            bd.context_window,
            80,
            Some(legend_rows_count),
        );
        let mut context = pa::map_context_breakdown(&bd);
        context.cas = cas;
        self.send_response(io, req_id, ResponsePayload::Context(context))
            .await
    }

    /// The session's stored-block account in the wire form the context view
    /// reads, or None when the session used the store not at all.
    ///
    /// The folded half comes from the session log, so a resumed session reports
    /// what its previous process recorded. The index half comes from the
    /// catalog the search tool answers through; a catalog failure leaves the
    /// index unknown rather than reporting an empty one.
    async fn cas_usage(&self) -> Option<CasUsage> {
        let summary = self
            .runner
            .store()
            .trajectory_head(self.session)
            .summary
            .cas;
        if !reports_cas(&summary) {
            return None;
        }
        Some(map_cas_usage(&summary, None))
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
                let reply = map_compact_reply(&outcome);
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
