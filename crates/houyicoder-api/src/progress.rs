//! The tool progress port: the engine-facing contract a tool uses to report
//! progress to the host while it runs. Object-safe so a tool holds an
//! Arc<dyn ProgressSink> wired in by the agent loop; the concrete sink lives
//! in the host layer (a TUI status line, a structured log, or a no-op for
//! non-interactive runs). The agent loop constructs the sink and threads it
//! through the ToolCtx; a tool calls it from inside a long-running execute.

/// Progress reporting a tool drives during execute. The sink is intentionally
/// minimal: a human-readable message plus optional numeric progress. Every
/// method has a no-op default so a host that surfaces nothing (a test, a
/// non-interactive run) implements the trait with an empty body.
pub trait ProgressSink: Send + Sync {
    /// Emit a free-text status line the host surfaces while the tool runs.
    fn report(&self, _message: &str) {}
    /// Optional numeric progress: current units done out of an optional total.
    /// A None total signals indeterminate progress (a bar that pulses).
    fn progress(&self, _current: u64, _total: Option<u64>) {}
}

/// A no-op sink for non-interactive runs and tests that do not surface
/// progress. report and progress are default no-ops; a host wires a real
/// sink only when there is something to show.
pub struct NoProgress;

impl ProgressSink for NoProgress {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trait_is_object_safe() {
        let _sink: Box<dyn ProgressSink> = Box::new(NoProgress);
    }
}
