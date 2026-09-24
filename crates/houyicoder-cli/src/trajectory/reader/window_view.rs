//! The view the pane draws for the window in hand.
//!
//! The window is the resident pages; the view is what the pane renders from
//! them. It is built once and then served from the cache, because the pane draws
//! every frame and re-projecting a page per frame would put a fold on the draw
//! path. The head's summary supplies the session's own figures: a page holds the
//! newest turns, and its totals would report the page as the session.

use super::super::view::project_rows;
use super::{DELTA_MAX_BYTES, PageRead, SessionHistory};
use super::{DurableWatermark, SessionLogTrajectory, TrajectoryHead, TrajectoryState};
use houyicoder_context::{SessionEvent, SessionLogEntry};
use houyicoder_tui::view::trajectory_pane::{SessionTiming, SubagentUsage};
use houyicoder_tui::view::trajectory_pane::{TrajectoryRow, TrajectoryView, TrajectoryViewState};
use std::sync::Arc;

impl SessionLogTrajectory {
    /// The window's events in log order: older pages first, the tail last.
    ///
    /// A page whose oldest turn was cut short by the byte budget carries a
    /// fragment of a turn, so everything before the window's first user input
    /// is dropped: rendering it would invent a turn the session never had.
    fn window_events(state: &TrajectoryState) -> Vec<SessionLogEntry> {
        let mut events: Vec<SessionLogEntry> = state
            .pages
            .iter()
            .flat_map(|page| page.events.iter())
            .map(|located| located.entry.clone())
            .collect();
        if state.pages.front().is_some_and(|page| page.oldest_partial)
            && let Some(cut) = events
                .iter()
                .position(|entry| matches!(entry.event, SessionEvent::UserInput { .. }))
        {
            events.drain(..cut);
        }
        events
    }

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
        let events = Self::window_events(state);
        // Number from the rows the projection actually produced, not from the
        // user inputs in the events: a window that opens mid-run yields a turn
        // the fold numbers but no user input counts, and subtracting that turn
        // as if it were hidden would overstate what is behind the window.
        let mut rows = project_rows(&events, 1);
        let visible = rows
            .iter()
            .filter(|row| matches!(row, TrajectoryRow::Turn(_)))
            .count();
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
        value.skipped_records = state.pages.iter().map(|page| page.skipped).sum();
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
    let end = back.end_offset;
    if end > 0 && size > end && size - end <= DELTA_MAX_BYTES {
        PageRead::Append {
            from: end,
            to: size,
        }
    } else {
        PageRead::Tail { to: size }
    }
}
