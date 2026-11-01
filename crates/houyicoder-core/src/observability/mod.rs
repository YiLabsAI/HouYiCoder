//! Observability: cross-cutting collection layer and its read-only views.
//!
//! The log gathers records from every subsystem without mutating its state;
//! the MetricsView trait exposes the contents as read-only snapshots for
//! inspection commands. Self-evolution records (experience, lesson, skill,
//! reflection, failure, cross-run link) are types in the evolution submodule;
//! storage and integration are not built yet. The failure record anchors into
//! the raw trajectory log, and multilevel caps bound every field.

pub mod evolution;

mod export;
mod latency;
mod log;
mod tool_stats;
mod usage;

#[cfg(test)]
mod tests;

pub use export::{ExportError, ExportSink};
pub use latency::LatencyHistogram;
pub use log::{MetricsView, ObservabilityLog};
pub use tool_stats::{ToolCallRecord, tool_failure_reason};
pub use usage::{CostAccumulator, CostSummary, ModelUsage, NoCharge, PricingTable, TurnTokenDelta};

/// Truncate a string to a char budget, appending an overflow marker when
/// cut. Counts chars, not bytes, so multi-byte boundaries are respected.
pub(crate) fn truncate_str(s: &str, cap: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= cap {
        return s.to_string();
    }
    let mut t: String = chars[..cap].iter().collect();
    let extra = chars.len() - cap;
    t.push_str(&format!(" (+{extra} chars)"));
    t
}
