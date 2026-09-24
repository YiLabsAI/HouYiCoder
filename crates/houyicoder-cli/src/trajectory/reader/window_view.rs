//! The view the pane draws for the window in hand.
//!
//! The window is the resident pages; the view is what the pane renders from
//! them. It is built once and then served from the cache, because the pane draws
//! every frame and folding a page per frame would put a fold on the draw
//! path. The head's summary supplies the session's own figures: a page holds the
//! newest turns, and its totals would report the page as the session.

use super::{DELTA_MAX_BYTES, PageRead, SessionHistory};
use super::{
    DurableWatermark, ResidentPage, SessionLogTrajectory, TrajectoryHead, TrajectoryState,
};
use houyicoder_tui::view::trajectory_pane::{
    ModelSwitchBoundary, SessionTiming, SubagentUsage, TurnBoundary,
};
use houyicoder_tui::view::trajectory_pane::{TrajectoryRow, TrajectoryView, TrajectoryViewState};
use std::collections::VecDeque;
use std::sync::Arc;

impl SessionLogTrajectory {
    /// Build the view for the window in hand: rows from the page, every
    /// session figure from the head's summary.
    ///
    /// The header answers what the session spent, so it cannot be computed
    /// from the page: a page holds the newest turns, and its own totals would
    /// report the page as the session.
    pub(super) fn build_view(
        &self,
        state: &mut TrajectoryState,
        head: &TrajectoryHead,
        load: TrajectoryViewState,
    ) -> Arc<TrajectoryView> {
        if state.pages.is_empty() {
            // Cached like any other state: a settled empty session would
            // otherwise build a fresh view on every frame.
            let view = self.head_view(head, load, state.history_generation);
            state.view = Some(Arc::clone(&view));
            return view;
        }
        let mut rows = seam_rows(&state.pages);
        let visible = rows.len();
        // While the window ends at the tail it hides only what is behind it, so
        // the count is derived, from the total the window was read at: the
        // session's total moves on every append, and numbering a page still in
        // hand from it would rename every row in the window. Once the user
        // walks away, the count is the one they left it at, moved by each page
        // that arrived, because the turns in front are the ones being dropped.
        let older_hidden = if state.follow_tail {
            state.window_total.saturating_sub(visible)
        } else {
            state.older_hidden
        };
        state.older_hidden = older_hidden;
        let newer_hidden = head
            .summary
            .total_turns
            .saturating_sub(older_hidden + visible);
        let mut number = older_hidden;
        for row in rows.iter_mut() {
            if let TrajectoryRow::Turn(turn) = row {
                number += 1;
                turn.n = number;
            }
        }
        let mut view = self.head_view(head, load, state.history_generation);
        let value = Arc::make_mut(&mut view);
        value.hidden_turns = older_hidden;
        value.newer_hidden = newer_hidden;
        value.skipped_records = state.pages.iter().map(|page| page.source.skipped).sum();
        value.rows = rows;
        state.view = Some(Arc::clone(&view));
        view
    }

    /// The view for a state, built once and then served from the cache.
    ///
    /// A draw must not rebuild the window: while a read is in flight the state
    /// is unchanged frame to frame, and re-projecting the page each time would
    /// put a page fold on the draw path.
    pub(super) fn serve(
        &self,
        state: &mut TrajectoryState,
        head: &TrajectoryHead,
        load: TrajectoryViewState,
        watermark: DurableWatermark,
    ) -> Arc<TrajectoryView> {
        if let Some(view) = state.view.as_ref()
            && state.view_state == Some(load)
            && state.view_watermark == Some(watermark)
        {
            return Arc::clone(view);
        }
        let view = self.build_view(state, head, load);
        state.view_state = Some(load);
        state.view_watermark = Some(watermark);
        view
    }

    /// A view that carries the session's figures and no rows, for the states
    /// where there is nothing truthful to list yet.
    pub(super) fn head_view(
        &self,
        head: &TrajectoryHead,
        load: TrajectoryViewState,
        generation: u64,
    ) -> Arc<TrajectoryView> {
        let summary = &head.summary;
        Arc::new(TrajectoryView {
            session_id: self.session_id.to_string(),
            // The session's own model count, not the construction-time string:
            // a session that switched models must say so.
            model: match summary.models_used {
                0 => self.model.clone(),
                1 => summary
                    .single_model
                    .as_ref()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| self.model.clone()),
                n => format!("{n} models"),
            },
            total_turns: summary.total_turns,
            models_used: summary.models_used,
            tokens_in: summary
                .usage
                .totals_known
                .then_some(summary.usage.input_tokens as usize),
            tokens_out: summary
                .usage
                .totals_known
                .then_some(summary.usage.output_tokens as usize),
            cache_read: (summary.usage.cache_read_tokens > 0)
                .then_some(summary.usage.cache_read_tokens),
            failures: summary.usage.failures,
            tool_calls: summary.usage.tool_calls,
            duration_secs: summary.duration_ms / 1000,
            timing: SessionTiming {
                ttft_samples: summary.timing.ttft_samples,
                ttft_avg_ms: summary.timing.ttft_avg_ms,
                ttft_p95_ms: summary.timing.ttft_p95_ms,
                ttft_p99_ms: summary.timing.ttft_p99_ms,
                decode_samples: summary.timing.decode_samples,
                decode_tok_per_sec: summary.timing.decode_tok_per_sec,
                model_ms: summary.timing.model_ms,
                tool_ms: summary.timing.tool_ms,
            },
            hidden_turns: summary.total_turns,
            newer_hidden: 0,
            history_generation: generation,
            subagent_usage: (summary.usage.subagent.calls > 0).then_some(SubagentUsage {
                calls: summary.usage.subagent.calls,
                input: summary.usage.subagent.input_tokens,
                output: summary.usage.subagent.output_tokens,
                cache_read: summary.usage.subagent.cache_read_input_tokens,
            }),
            state: load,
            skipped_records: 0,
            rows: Vec::new(),
        })
    }
}

/// The pages' rows stitched into the window, carrying what each page leaves for
/// the one after it across the seam.
///
/// A boundary the log recorded between two turns, and the model the last call
/// used, can sit between two pages: only reading the two together names the
/// turn that follows them the way a fold over both would.
pub(in crate::trajectory) fn seam_rows(pages: &VecDeque<ResidentPage>) -> Vec<TrajectoryRow> {
    let mut rows: Vec<TrajectoryRow> = Vec::new();
    let mut carried_boundaries: Vec<TurnBoundary> = Vec::new();
    let mut carried_model: Option<String> = None;
    for page in pages.iter() {
        let mut page_rows = page.accumulator.rows();
        let drew_rows = !page_rows.is_empty();
        if let Some(TrajectoryRow::Turn(first)) = page_rows.first_mut() {
            if !carried_boundaries.is_empty() {
                let mut boundaries = std::mem::take(&mut carried_boundaries);
                boundaries.append(&mut first.boundary_before);
                first.boundary_before = boundaries;
            }
            // A switch across the seam is dated by the call that shows it.
            if let (Some(prev), Some((next, ts))) =
                (carried_model.as_ref(), page.accumulator.first_usage())
                && prev != next
            {
                first
                    .boundary_before
                    .push(TurnBoundary::ModelSwitch(Box::new(ModelSwitchBoundary {
                        from: prev.clone(),
                        to: next.clone(),
                        at_secs: ts / 1000,
                    })));
            }
        }
        // A page that drew no rows carries nothing of its own: what the page
        // before it left must not be dropped by an empty page between them, so
        // the two are merged rather than replaced.
        let (trailing, last_usage) = page.accumulator.trailing();
        if drew_rows {
            carried_boundaries = trailing;
        } else {
            carried_boundaries.extend(trailing);
        }
        if let Some((model, _)) = last_usage {
            carried_model = Some(model);
        }
        rows.extend(page_rows);
    }
    rows
}

/// Which read a window that follows the tail needs when the history moved.
///
/// The common case is that the session appended: the window can take what the
/// log added after the byte it ends at, instead of reading a whole page again
/// for it. A burst bigger than the delta budget, or a window with no end to
/// start from, is read as the tail.
pub(super) fn append_or_tail(history: &SessionHistory, state: &TrajectoryState) -> PageRead {
    let size = history.log_size();
    let Some(back) = state.pages.back() else {
        return PageRead::Tail { to: size };
    };
    let end = back.source.end_offset;
    if end > 0 && size > end && size - end <= DELTA_MAX_BYTES {
        PageRead::Append {
            from: end,
            to: size,
        }
    } else {
        PageRead::Tail { to: size }
    }
}
