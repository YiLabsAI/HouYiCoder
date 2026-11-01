//! Progress reporting driven by tools during execution.

/// One progress snapshot from an executing tool.
///
/// Structured rather than free text because the running row renders every
/// field it carries: the elapsed seconds, the call's own budget, and the last
/// output lines, none of which a sentence would let the host read back out.
/// The fields are additive for reporters: one that renders only some of them
/// ignores the rest, and the default implementation renders none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolProgressUpdate {
    /// Whole seconds since the call began.
    pub elapsed_secs: u64,
    /// The call's own wall-clock budget in whole seconds, when it declared
    /// one. This is the command's limit, not the foreground budget.
    pub timeout_secs: Option<u64>,
    /// The most recent output lines, oldest first, for the running preview.
    pub tail: Vec<String>,
}

/// Reports progress from an executing tool.
pub trait ToolProgressReporter: Send + Sync {
    /// Report one progress snapshot.
    fn update(&self, _update: ToolProgressUpdate) {}
}

/// A reporter that intentionally ignores tool progress.
pub struct NoToolProgress;

impl ToolProgressReporter for NoToolProgress {}

#[cfg(test)]
mod tests;
