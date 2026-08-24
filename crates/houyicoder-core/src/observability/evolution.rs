//! Self-evolution record types.
//!
//! Six record types form the cross-run experience substrate: experience,
//! lesson, skill, reflection, failure, and cross-run link. These are defined
//! as types here; storage and wiring are not built yet — the types are
//! defined so the rest of this module can reference them.
//!
//! The failure record carries a zero-copy TrajectoryRef pointer into the raw
//! trajectory log rather than duplicating state — the full event state
//! already lives in the immutable append-only log. Multi-level caps on every
//! bounded field (error message, affected paths, failure reasons, fix chain)
//! prevent unbounded growth from pathological failure storms.

use std::path::PathBuf;

use houyicoder_context::EventId;
use serde::{Deserialize, Serialize};

use super::truncate_str;

// ===== bounds =====

/// Maximum chars of an error message stored in a failure record.
pub(crate) const FAILURE_MSG_CAP: usize = 500;
/// Maximum file paths a failure record references.
pub(crate) const AFFECTED_PATHS_CAP: usize = 10;
/// Maximum distinct per-attempt reasons inside a single failure episode.
pub(crate) const FAILURE_REASONS_PER_EPISODE_CAP: usize = 5;
/// Maximum causal links in a fix chain before truncation.
pub(crate) const FIX_CHAIN_CAP: usize = 8;

// ===== trajectory reference =====

/// Zero-copy pointer into the raw trajectory log. The full event state lives
/// in the immutable append-only log; this ref locates a range so a consumer
/// rehydrates on demand without duplicating bytes. Matches the context
/// layer block-ref pattern: anchor plus reference, not copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrajectoryRef {
    pub turn_id: u32,
    pub event_range: (EventId, EventId),
}

// ===== failure record =====

/// Bounded category of a failure for downstream analysis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCategory {
    ToolError,
    ProviderError,
    SandboxError,
    Panic,
    Unknown,
}

/// The arbitration a failure escalated to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AskOrDeny {
    Ask,
    Deny,
}

/// Terminal state of a failure episode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureOutcome {
    /// A subsequent edit resolved the failure.
    Recovered { fix_turn: u32 },
    /// The failure was escalated to user arbitration.
    Escalated { to: AskOrDeny },
    /// The episode was abandoned without resolution.
    Abandoned,
}

/// Confidence tier for a causal fix link, from weak co-occurrence to
/// verified re-execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FixConfidence {
    /// Path co-occurrence only (cheap, always available).
    Suspected,
    /// Symbol or line-range match between the error and the edit diff.
    Likely,
    /// Re-execution of the failing command succeeded after the edit.
    Verified,
    /// User or skill explicitly attributed the fix.
    Manual,
}

/// Evidence backing a fix-link confidence tier. All variants are bounded
/// references or short tokens, not full state copies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FixEvidence {
    PathMatch {
        path: PathBuf,
    },
    SymbolLineMatch {
        token: String,
        line_range: (u32, u32),
    },
    ReExecSuccess {
        re_run: TrajectoryRef,
    },
    AttributionNote {
        credit: String,
    },
}

/// One link in a causal fix chain: an edit that may address a prior error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixLink {
    /// Pointer to the edit event in the trajectory log.
    pub edit: TrajectoryRef,
    pub error_addressed: String,
    pub turn: u32,
    pub confidence: FixConfidence,
    pub evidence: Vec<FixEvidence>,
}

/// Composite identifier linking a failure episode to a tool call and turn.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FailureId {
    pub tool_name: String,
    pub call_id: String,
    pub turn: u32,
}

/// A failure episode: tool failed, possibly retried, then recovered,
/// escalated, or abandoned. Built incrementally (start at failure, append
/// retries, close at resolution). The post-failure state is a TrajectoryRef
/// (zero-copy pointer), not a snapshot — the full state already lives in
/// the trajectory raw log. Bounded fields prevent pathological growth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureRecord {
    pub failure_id: FailureId,
    pub tool_name: String,
    pub attempt_count: u32,
    /// All attempt call_ids in this episode (the model mints one per retry;
    /// the FailureId.call_id is the first). fix_chain + ExPeL mining read
    /// these to replay each attempt. Bounded by the episode's retry count.
    pub attempt_call_ids: Vec<String>,
    /// Per-attempt (attempt number, reason) pairs, deduplicated and capped.
    pub failure_reasons: Vec<(u32, String)>,
    /// Zero-copy pointer into the trajectory log.
    pub state_ref: TrajectoryRef,
    pub error_type: ErrorCategory,
    /// Truncated error message (cap enforced at write).
    pub error_message: String,
    /// Paths the failure touched or referenced (cap enforced at write).
    pub affected_paths: Vec<PathBuf>,
    /// Causal fix chain (cap enforced at write).
    pub fix_chain: Vec<FixLink>,
    pub outcome: FailureOutcome,
    pub recovery_turn: Option<u32>,
}

impl FailureRecord {
    /// Cap failure reasons to the per-episode limit, keeping the most
    /// recent entries.
    pub fn cap_reasons(&mut self) {
        if self.failure_reasons.len() > FAILURE_REASONS_PER_EPISODE_CAP {
            let cut = self.failure_reasons.len() - FAILURE_REASONS_PER_EPISODE_CAP;
            self.failure_reasons.drain(..cut);
        }
    }

    /// Cap the fix chain to the link limit, keeping the most recent links.
    pub fn cap_fix_chain(&mut self) {
        if self.fix_chain.len() > FIX_CHAIN_CAP {
            let cut = self.fix_chain.len() - FIX_CHAIN_CAP;
            self.fix_chain.drain(..cut);
        }
    }

    /// Truncate the error message to the configured cap, appending an
    /// overflow marker when cut.
    pub fn truncate_message(&mut self) {
        self.error_message = truncate_str(&self.error_message, FAILURE_MSG_CAP);
    }

    /// Cap the affected-paths list, keeping the most recent entries.
    pub fn cap_paths(&mut self) {
        if self.affected_paths.len() > AFFECTED_PATHS_CAP {
            let cut = self.affected_paths.len() - AFFECTED_PATHS_CAP;
            self.affected_paths.drain(..cut);
        }
    }

    /// Apply all caps in one call.
    pub fn apply_all_caps(&mut self) {
        self.cap_reasons();
        self.cap_fix_chain();
        self.truncate_message();
        self.cap_paths();
    }
}

// ===== other self-evolution records =====

/// Cross-run experience summary: what happened in one run, with lineage
/// to parent runs for experiment tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperienceRecord {
    pub run_id: String,
    pub cohort_id: Option<String>,
    pub task: String,
    pub outcome: RunOutcome,
    pub token_cost: houyicoder_protocol::llm::Usage,
    pub wall_clock_ms: u64,
    pub parent_run_id: Option<String>,
}

/// Terminal state of a cross-run experiment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunOutcome {
    Success,
    Failure,
    Halted,
}

/// A natural-language lesson learned across runs, with an importance count
/// and a lifecycle (added, edited, or removed). Linked to failure ids and
/// task tags for retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LessonRecord {
    pub lesson: String,
    pub importance: u32,
    pub lifecycle: LessonLifecycle,
    pub failure_ids: Vec<FailureId>,
    pub task_tags: Vec<String>,
}

/// The lifecycle state of a lesson entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LessonLifecycle {
    Added,
    Edited,
    Removed,
}

/// A reusable code pattern or skill, with an embedding for retrieval and a
/// success-rate tracker. Versioned so improvements supersede, not delete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRecord {
    pub description: String,
    pub code: String,
    pub embedding: Option<Vec<f32>>,
    pub success_rate: f64,
    pub version: u32,
}

/// Free-text reflection on a run or span, linked to trajectory events and
/// carrying a root-cause hypothesis and the action taken.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflectionRecord {
    pub text: String,
    pub trajectory_span: TrajectoryRef,
    pub root_cause_hypothesis: String,
    pub action_taken: String,
}

/// Cross-run lineage link: how one run evolved from another, with the
/// diff of what changed (prompt, tools, lessons added or removed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossRunLink {
    pub experiment_id: String,
    pub from_run_id: String,
    pub to_run_id: String,
    pub change_diff: String,
}

// ===== redundant call record =====

/// Maximum chars of the input preview stored in a redundant-call record.
/// 200 = enough to identify the tool input at a glance; lower = harder to
/// diagnose, higher = more memory per record (× 256 records in the ring).
pub(crate) const REDUNDANCY_INPUT_PREVIEW_CAP: usize = 200;
/// Maximum redundant-call records kept in the live ring buffer. A session
/// stuck in a re-read loop produces a storm of these; the cap drops oldest so
/// the buffer stays bounded (matches the failure-record storm caps). 256 =
/// enough for the /trajectory pane to show a full session's recent redundancy
/// pattern; lower = older patterns invisible; higher = more memory but no
/// functional gain past a typical session's call count.
pub(crate) const REDUNDANCY_RECORDS_CAP: usize = 256;

/// Why a redundant call was flagged. Drives the trajectory display + the
/// self-evolution reward signal (a SameBatch cluster is a stronger nudge
/// than a CrossBatch re-read).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RedundancyKind {
    /// Two calls with identical input in one assistant message (one batch),
    /// before either executes. The strongest signal — even "context loss"
    /// can't explain a same-message self-repeat.
    SameBatch,
    /// A later call repeats a prior (different batch) call's input, with no
    /// intervening write to the affected path. Often a context-loss re-read
    /// (compaction dropped the prior result) or a cognitive loop.
    CrossBatch,
}

/// One redundant tool-call observation: the model re-issued a call whose
/// input matches a prior call, with no intervening write. A self-evolution
/// signal (reward target), not a security gate — the tracker records + logs;
/// a future /trajectory pane surfaces the pattern, and a future PreToolUse
/// Feedback nudge can steer the model. Sibling to FailureRecord: both are
/// cross-run experience substrate pointing at the trajectory log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedundantCall {
    pub tool: String,
    /// Canonical input preview (capped), for the trajectory display + log.
    pub input_preview: String,
    pub kind: RedundancyKind,
    /// Calls between the prior same-input call and this one (0 for SameBatch).
    pub gap: u64,
    /// The seq of the prior call this one duplicates.
    pub last_seq: u64,
    /// Locates the prior call in the trajectory log. None until the
    /// /trajectory pane wiring fills the event range.
    pub prior_ref: Option<TrajectoryRef>,
}
