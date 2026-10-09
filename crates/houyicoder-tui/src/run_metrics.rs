//! The session's cumulative run metric tally, reset on session switch.

use std::time::Instant;

/// The session's cumulative run metrics: when its first run started, and the
/// output tokens and model-call steps accrued across its completed runs. The
/// run completion path records each final run here; a session switch rebuilds
/// it with defaults, so the tally spans one session while outliving any one
/// run and its return to idle.
#[derive(Default)]
pub(crate) struct RunMetrics {
    /// When the session's first run started, for end-to-end elapsed.
    pub(crate) session_started_at: Option<Instant>,
    /// Cumulative output tokens across all turns this session.
    pub(crate) cumulative_tokens: u64,
    /// Cumulative model-call steps across all turns.
    pub(crate) cumulative_steps: u32,
}
