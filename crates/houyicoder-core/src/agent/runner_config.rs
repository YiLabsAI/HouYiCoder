//! Runner configuration + snapshot retention defaults. Split out of mod.rs
//! so the agent module stays under the file-size cap.

use houyicoder_resilience::Retry;

// The output-token default is owned by the model-window module (the
// per-family resolver), since the cap is a model capability. Re-exported here
// so RunnerConfig::default + historical call sites reference one place.
pub use super::model_window::DEFAULT_MAX_OUTPUT_TOKENS;

/// Default snapshot retention: seven days. Snapshots older than this are
/// pruned at the start of each run. Overridable via Runner.
pub const DEFAULT_SNAPSHOT_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Default snapshot store size cap: one gibibyte. When the store exceeds this
/// the oldest snapshots are pruned (undo-stack-referenced ones protected).
pub const DEFAULT_SNAPSHOT_SIZE_CAP_BYTES: u64 = 1024 * 1024 * 1024;

/// Runner configuration. Defaults: max 10 turns (lightweight), a 32k
/// output cap, the standard Retry backoff. The TUI overrides max_turns
/// to 50.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// The model id passed to the provider.
    pub model: String,
    /// Static system instructions resolved before each call. Dynamic
    /// instructions (functions of context) land when prompt management does.
    pub instructions: String,
    /// Max model calls per run before MaxTurnsReached (50 for the TUI; the
    /// default 10 is for lightweight test configs). A convergence reminder is
    /// injected a few turns before the cap so the model synthesizes and answers
    /// rather than looping until the hard limit.
    pub max_turns: u32,
    /// Output-token cap sent as max_tokens to the provider. A coding agent
    /// routinely emits long multi-file replies, so the old 8k default cut the
    /// model mid-sentence (the provider returned finish_reason length and the
    /// caller treated it as a natural stop). 32k is generous for normal turns
    /// while staying within common model output limits. Resolved per-model
    /// from the family catalog at the composition root.
    pub max_output_tokens: u32,
    /// Retry policy for transient provider errors.
    pub retry: Retry,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            model: "test".to_string(),
            instructions: String::new(),
            max_turns: 10,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            retry: Retry::default(),
        }
    }
}
