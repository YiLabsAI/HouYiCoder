//! Progress reporting driven by tools during execution.

/// Reports free-text and numeric progress from an executing tool.
pub trait ToolProgressReporter: Send + Sync {
    /// Report a free-text status update.
    fn report(&self, _message: &str) {}

    /// Report completed units and an optional total.
    fn progress(&self, _current: u64, _total: Option<u64>) {}
}

/// A reporter that intentionally ignores tool progress.
pub struct NoToolProgress;

impl ToolProgressReporter for NoToolProgress {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reporter_supports_trait_objects() {
        let _reporter: Box<dyn ToolProgressReporter> = Box::new(NoToolProgress);
    }
}
