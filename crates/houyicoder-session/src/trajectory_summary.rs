//! The session-wide trajectory summary, folded once per durable event.
//!
//! The trajectory header and the status pane report figures about every turn
//! the session ran, while the pane loads only a window of turns, so those
//! figures cannot come from the window. This accumulator folds each event as
//! the store takes it, which keeps a read to a copy of small numbers rather
//! than a scan of the log.

use std::collections::{HashMap, HashSet};

use houyicoder_api::session::{
    SubagentUsage, TrajectorySummary, TrajectoryTimingSummary, TrajectoryUsageSummary,
};
use houyicoder_context::{SessionEvent, SessionLogEntry};
use houyicoder_protocol::tool::tool_result_failed;
use serde_json::Value;

/// Buckets for the fine range, 50 ms each up to 10 s.
const TTFT_FINE_BUCKETS: usize = 200;
/// Buckets for the coarse range, 250 ms each up to 60 s.
const TTFT_COARSE_BUCKETS: usize = 200;
/// Buckets for the wide range, 5 s each up to 600 s.
const TTFT_WIDE_BUCKETS: usize = 108;
/// Buckets in total: the three ranges plus one for anything beyond them.
const TTFT_BUCKETS: usize = TTFT_FINE_BUCKETS + TTFT_COARSE_BUCKETS + TTFT_WIDE_BUCKETS + 1;

/// The width of the fine range in ms.
const TTFT_FINE_MS: u64 = 50;
/// Where the coarse range starts, in ms.
const TTFT_COARSE_START_MS: u64 = TTFT_FINE_MS * TTFT_FINE_BUCKETS as u64;
/// The width of the coarse range in ms.
const TTFT_COARSE_MS: u64 = 250;
/// Where the wide range starts, in ms.
const TTFT_WIDE_START_MS: u64 = TTFT_COARSE_START_MS + TTFT_COARSE_MS * TTFT_COARSE_BUCKETS as u64;
/// The width of the wide range in ms.
const TTFT_WIDE_MS: u64 = 5_000;

/// A time-to-first-token distribution held in fixed buckets.
///
/// Recording is constant time and a percentile walks a fixed number of
/// buckets, so neither grows with the session. The price is precision: a
/// percentile is the upper bound of the bucket that holds the rank, so it is
/// reported to the width of that bucket. The average stays exact.
struct TtftHistogram {
    buckets: [u32; TTFT_BUCKETS],
    samples: u64,
    sum_ms: u64,
}

impl Default for TtftHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; TTFT_BUCKETS],
            samples: 0,
            sum_ms: 0,
        }
    }
}

impl TtftHistogram {
    fn record(&mut self, ms: u64) {
        let index = bucket_of(ms);
        self.buckets[index] = self.buckets[index].saturating_add(1);
        self.samples = self.samples.saturating_add(1);
        self.sum_ms = self.sum_ms.saturating_add(ms);
    }

    /// The upper bound of the bucket holding the given rank, and whether that
    /// bucket is the overflow one, whose true value is at least the bound.
    fn percentile(&self, p: f64) -> Option<(u64, bool)> {
        if self.samples == 0 {
            return None;
        }
        let target = ((self.samples as f64 * p).ceil() as u64).max(1);
        let mut seen: u64 = 0;
        for (i, count) in self.buckets.iter().enumerate() {
            seen += u64::from(*count);
            if seen >= target {
                let overflow = i == TTFT_BUCKETS - 1;
                return Some((bucket_upper_bound(i), overflow));
            }
        }
        Some((bucket_upper_bound(TTFT_BUCKETS - 1), true))
    }
}

/// The bucket a latency falls in.
fn bucket_of(ms: u64) -> usize {
    if ms < TTFT_COARSE_START_MS {
        (ms / TTFT_FINE_MS) as usize
    } else if ms < TTFT_WIDE_START_MS {
        TTFT_FINE_BUCKETS + ((ms - TTFT_COARSE_START_MS) / TTFT_COARSE_MS) as usize
    } else if ms < TTFT_WIDE_START_MS + TTFT_WIDE_MS * TTFT_WIDE_BUCKETS as u64 {
        TTFT_FINE_BUCKETS
            + TTFT_COARSE_BUCKETS
            + ((ms - TTFT_WIDE_START_MS) / TTFT_WIDE_MS) as usize
    } else {
        TTFT_BUCKETS - 1
    }
}

/// The largest latency the bucket can hold.
fn bucket_upper_bound(index: usize) -> u64 {
    if index < TTFT_FINE_BUCKETS {
        (index as u64 + 1) * TTFT_FINE_MS
    } else if index < TTFT_FINE_BUCKETS + TTFT_COARSE_BUCKETS {
        TTFT_COARSE_START_MS + (index - TTFT_FINE_BUCKETS + 1) as u64 * TTFT_COARSE_MS
    } else if index < TTFT_BUCKETS - 1 {
        TTFT_WIDE_START_MS
            + (index - TTFT_FINE_BUCKETS - TTFT_COARSE_BUCKETS + 1) as u64 * TTFT_WIDE_MS
    } else {
        TTFT_WIDE_START_MS + TTFT_WIDE_MS * TTFT_WIDE_BUCKETS as u64
    }
}

/// What a tool call carried, so a later result can be judged against it.
struct ToolCallFacts {
    tool: String,
    input: Value,
}

/// The session-wide trajectory summary, folded one event at a time.
#[derive(Default)]
pub(crate) struct TrajectorySummaryState {
    user_inputs: usize,
    /// True when the log's first event is not a user input, which the fold
    /// numbers as a turn of its own.
    leading_partial_turn: bool,
    seen_any_event: bool,

    /// Turns that reached their end without reporting usage, so the token
    /// total is a lower bound.
    closed_turns_without_usage: usize,
    /// Whether a turn is open, and whether it has reported usage yet.
    turn_open: bool,
    open_turn_has_usage: bool,

    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    failures: usize,
    subagent: SubagentUsage,

    first_ts: Option<u64>,
    last_ts: Option<u64>,

    model_ms: u64,
    tool_ms: u64,
    decode_samples: usize,
    decode_tokens: u64,
    decode_ms: u64,
    ttft: TtftHistogram,

    models: HashSet<String>,
    /// Calls whose result has not arrived. Retired when the turn that issued
    /// them ends, so a long session does not accumulate them.
    pending_calls: HashMap<String, ToolCallFacts>,
}

impl TrajectorySummaryState {
    /// Fold one event. Streaming deltas advance the span but no counter: they
    /// are not durable, so counting them would make a live session's figures
    /// differ from the same session read back.
    pub(crate) fn record(&mut self, entry: &SessionLogEntry) {
        if !self.seen_any_event {
            self.seen_any_event = true;
            self.leading_partial_turn = !matches!(entry.event, SessionEvent::UserInput { .. });
            if self.leading_partial_turn {
                // The fold numbers this as a turn, so it must be open for the
                // usage check to see that it reported none.
                self.turn_open = true;
            }
        }
        if self.first_ts.is_none() {
            self.first_ts = Some(entry.ts);
        }
        self.last_ts = Some(entry.ts);

        match &entry.event {
            SessionEvent::AssistantTextDelta { .. } => {}
            SessionEvent::UserInput { .. } => {
                self.close_turn();
                self.user_inputs = self.user_inputs.saturating_add(1);
                self.turn_open = true;
                self.open_turn_has_usage = false;
            }
            SessionEvent::TurnUsage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                model,
                ..
            } => {
                self.open_turn_has_usage = true;
                self.input_tokens = self.input_tokens.saturating_add(*input_tokens);
                self.output_tokens = self.output_tokens.saturating_add(*output_tokens);
                self.cache_read_tokens = self
                    .cache_read_tokens
                    .saturating_add(*cache_read_input_tokens);
                // Decode speed is measured against the tokens the model
                // produced, which is what the output count reports.
                self.decode_tokens = self.decode_tokens.saturating_add(*output_tokens);
                if !model.is_empty() {
                    self.models.insert(model.clone());
                }
            }
            SessionEvent::ModelStepTiming {
                total_ms,
                ttft_ms,
                decode_ms,
                ..
            } => {
                if let Some(ttft) = ttft_ms {
                    self.ttft.record(*ttft);
                }
                if let Some(decode) = decode_ms {
                    self.decode_ms = self.decode_ms.saturating_add(*decode);
                    self.decode_samples = self.decode_samples.saturating_add(1);
                }
                self.model_ms = self.model_ms.saturating_add(*total_ms);
            }
            SessionEvent::ToolCall {
                call_id,
                tool,
                input,
            } => {
                self.pending_calls.insert(
                    call_id.clone(),
                    ToolCallFacts {
                        tool: tool.clone(),
                        input: input.clone(),
                    },
                );
            }
            SessionEvent::ToolResult {
                call_id,
                output,
                duration_ms,
            } => {
                let facts = self.pending_calls.remove(call_id);
                let (tool, input) = match &facts {
                    Some(facts) => (facts.tool.as_str(), &facts.input),
                    // A result whose call sits outside what this fold has
                    // seen: the plain error-or-success rule still applies.
                    None => ("", &Value::Null),
                };
                if tool_result_failed(output, tool, input) {
                    self.failures = self.failures.saturating_add(1);
                }
                self.tool_ms = self.tool_ms.saturating_add(*duration_ms);
            }
            SessionEvent::SubagentReturn { .. } => self.subagent.record(&entry.event),
            SessionEvent::TurnAborted { .. }
            | SessionEvent::RunCompleted { .. }
            | SessionEvent::ContextCleared { .. } => self.close_turn(),
            _ => {}
        }
    }

    /// End the open turn: a turn that reported no usage leaves a hole in the
    /// session total, and its calls can no longer receive a result.
    fn close_turn(&mut self) {
        if self.turn_open && !self.open_turn_has_usage {
            self.closed_turns_without_usage = self.closed_turns_without_usage.saturating_add(1);
        }
        self.turn_open = false;
        self.open_turn_has_usage = false;
        self.pending_calls.clear();
    }

    /// Turns as the pane numbers them. A log that opens mid-run carries one
    /// more than it has user inputs.
    pub(crate) fn numbered_turns(&self) -> usize {
        self.user_inputs
            .saturating_add(usize::from(self.leading_partial_turn))
    }

    /// Whether every turn reported usage, so the token total is the session's
    /// cost rather than a lower bound.
    fn totals_known(&self) -> bool {
        let open_turn_is_measured = !self.turn_open || self.open_turn_has_usage;
        self.numbered_turns() > 0 && self.closed_turns_without_usage == 0 && open_turn_is_measured
    }

    pub(crate) fn snapshot(&self) -> TrajectorySummary {
        let decode_tok_per_sec = if self.decode_ms > 0 && self.decode_tokens > 0 {
            Some(self.decode_tokens as f64 / (self.decode_ms as f64 / 1000.0))
        } else {
            None
        };
        let (ttft_p95_ms, p95_capped) = split(self.ttft.percentile(0.95));
        let (ttft_p99_ms, p99_capped) = split(self.ttft.percentile(0.99));
        TrajectorySummary {
            total_turns: self.numbered_turns(),
            usage: TrajectoryUsageSummary {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cache_read_tokens: self.cache_read_tokens,
                totals_known: self.totals_known(),
                failures: self.failures,
                subagent: self.subagent,
                subagent_unmeasured: self.subagent.unmeasured_calls > 0,
            },
            timing: TrajectoryTimingSummary {
                ttft_samples: self.ttft.samples as usize,
                ttft_avg_ms: (self.ttft.samples > 0).then(|| self.ttft.sum_ms / self.ttft.samples),
                ttft_p95_ms,
                ttft_p99_ms,
                ttft_percentile_capped: p95_capped || p99_capped,
                decode_samples: self.decode_samples,
                decode_tok_per_sec,
                model_ms: self.model_ms,
                tool_ms: self.tool_ms,
            },
            models_used: self.models.len(),
            single_model: if self.models.len() == 1 {
                self.models.iter().next().cloned()
            } else {
                None
            },
            duration_ms: match (self.first_ts, self.last_ts) {
                (Some(first), Some(last)) => last.saturating_sub(first),
                _ => 0,
            },
        }
    }
}

/// Split a percentile into its value and whether it is a floor.
fn split(value: Option<(u64, bool)>) -> (Option<u64>, bool) {
    match value {
        Some((ms, capped)) => (Some(ms), capped),
        None => (None, false),
    }
}

#[cfg(test)]
#[path = "trajectory_summary_tests.rs"]
mod tests;
