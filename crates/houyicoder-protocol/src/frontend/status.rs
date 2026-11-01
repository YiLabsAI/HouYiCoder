//! The wire form of a runner status snapshot, returned to the frontend
//! over the wire so the TUI renders /status without importing the engine
//! crate. The engine StatusSnapshot carries a borrowed breaker_state label
//! and a Duration cool-down; the wire form owns both (a String and a
//! whole-second count) so it crosses the wire with no lifetime or
//! resilience-type leakage. Usage is the shared protocol llm type.
//!
//! The server optionally attaches a descriptor summary because the engine
//! snapshot cannot access the descriptor store.

use serde::{Deserialize, Serialize};

use crate::llm::{Usage, UsageProvenance};

/// A runner status snapshot, wire form. Owned + serde so it crosses any
/// carrier. The TUI renders this directly; it never sees the engine
/// StatusSnapshot or the resilience breaker type behind breaker_state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusSnapshot {
    /// The model id the runner sends in CompletionRequest.model.
    pub model: String,
    /// The breaker state as a render label ("Closed" / "Open" / "HalfOpen");
    /// None when the runner has no breaker.
    pub breaker_state: Option<String>,
    /// The last trip reason, pre-rendered to a human string, when the
    /// breaker is or was Open.
    pub breaker_reason: Option<String>,
    /// Remaining cool-down seconds when the breaker is Open and the
    /// cool-down has not elapsed; None otherwise.
    pub breaker_cool_down_secs: Option<u64>,
    /// Cumulative provider-reported usage across the whole session, including
    /// the turns a prior runner drove before a reopen.
    pub cumulative_usage: Usage,
    /// The input_tokens of the last recorded call: the provider's own count
    /// for it, whether this runner made that call or a reopen read it from the
    /// log. A billing fact, not the occupancy figure.
    pub last_input_tokens: u32,
    /// The token count of the latest assembled context, from the same
    /// measurement the context pane renders. None until a turn has assembled
    /// one — the pane previews what the first turn would cost instead of
    /// showing nothing, and that preview is not this field. Occupancy renders
    /// from here so the status bar and the context pane cannot disagree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used_tokens: Option<u32>,
    /// The provider-reported context window, from ModelCapabilities.
    pub context_window: u32,
    /// Total tool calls the model asked for across the session. Each one is
    /// either an execution (success or error), a refusal, or a call answered
    /// without running.
    pub tool_calls: u32,
    /// Tool executions that returned a value (not an error payload).
    pub tool_success: u32,
    /// Tool executions that errored (an error payload).
    pub tool_errors: u32,
    /// Tool calls refused before they ran, by the user or a hook. Defaulted
    /// so a payload from a server that predates the count still decodes.
    #[serde(default)]
    pub tool_refused: u32,
    /// The session identity fields attached by the server, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<SessionDescriptorSummary>,
    /// Where the api key came from, as a label: a configured keychain item or
    /// helper, the environment variable that supplied it, or None when nothing
    /// supplied one. The server resolves this from the same facts the provider
    /// was built with, so the TUI never imports the config crate and never
    /// sees the secret value (only the source name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token_source: Option<String>,
    /// The endpoint the provider actually sends completions to. The server
    /// carries this from the resolution rather than re-resolving it, so the TUI
    /// renders the real destination without importing the config crate.
    #[serde(default)]
    pub base_url: String,
    /// Which settings sources contribute ("User", "User, Project", or
    /// "(defaults)"). Resolved server-side so the TUI does not touch the
    /// settings file.
    #[serde(default)]
    pub setting_sources: String,
    /// The running build version, set by the server (env CARGO_PKG_VERSION).
    /// Always known, so /status shows Version even before the descriptor is
    /// materialized. Distinct from the descriptor's creation-version field,
    /// which carries the build that created the session.
    #[serde(default)]
    pub version: String,
    /// Whether turn-entry recall injection + the background extractor run.
    /// Read from the settings file server-side so the TUI never imports the
    /// config crate. Display-only: the user edits the settings file to flip it.
    #[serde(default = "default_true")]
    pub auto_memory: bool,
    /// Whether the background consolidation dream fires. Same provenance +
    /// display-only contract as auto_memory.
    #[serde(default = "default_true")]
    pub auto_dream: bool,
    /// Whether the host may write a recap while the user is away, and how long
    /// the session must sit idle first. Both come from the settings file
    /// server-side, the same way the two memory switches do, so the Config tab
    /// can show them without the TUI importing the config crate.
    #[serde(default)]
    pub recap_enabled: bool,
    #[serde(default)]
    pub recap_idle_secs: u64,
    /// Per-model token breakdown for the Usage sub-tab's "Usage by model"
    /// section. Sorted by input+output descending. Empty when the session
    /// has used a single model (the flat cumulative_usage already covers it)
    /// or no usage has been recorded. The server attaches this from the
    /// observability log's cost summary; the TUI renders it without
    /// importing the engine crate. Carries token counts only — no USD
    /// (no pricing source; see the observability design's cost decision).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_model: Vec<ModelUsageView>,
    /// The session's durable audit figures, folded from the session log so a
    /// restart or a reconnect reports the whole session rather than this
    /// process. All zero for a snapshot built without a session store.
    #[serde(default)]
    pub session_audit: SessionAuditView,
}

/// The session's approval and turn figures: what the session asked the user
/// and how much of the work finished. Execution results travel in the
/// snapshot's tool-call fields, which the server fills from the same durable
/// fold, so an execution block here would be a second copy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAuditView {
    pub approvals: ApprovalAuditView,
    pub turns: TurnAuditView,
}

/// What the session's approval cards asked and how each was answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalAuditView {
    /// Cards shown to the user, counted once per ask.
    pub cards: u32,
    /// Cards answered yes for the call they described only.
    pub approved_once: u32,
    /// Cards answered yes for the rest of this session.
    pub approved_session: u32,
    /// Cards answered yes and saved for the next start.
    pub approved_workspace: u32,
    /// Cards answered no.
    pub denied: u32,
    /// Cards that ended without a verdict: cancelled with Esc, abandoned at
    /// a disconnect, or interrupted before an answer. Not a denial, and
    /// reported apart so it cannot be read as one.
    pub unknown_outcome: u32,
}

/// How the session's turns ended, one count per turn. A turn counts by its
/// last terminal record, so a recovery that re-drives an interrupted turn is
/// counted as the way that later record ended it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnAuditView {
    /// Turns whose last terminal record states the model produced its answer.
    pub completed: u32,
    /// Turns whose last terminal record states they were cancelled.
    pub interrupted: u32,
    /// Turns that ended in another stated way: handed off, failed, rejected
    /// by the verify gate, or out of turn budget.
    pub unfinished: u32,
    /// Turns whose last terminal record states no outcome.
    pub unknown: u32,
}

/// Per-model token counts, wire form. A trimmed view of the engine
/// ModelUsage: only the token fields the Usage tab renders. No USD, no
/// context window, no max output tokens — those belong to the /model pane.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsageView {
    /// The model id, as sent in CompletionRequest.model.
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    /// Which of those counts the endpoints behind this model reported. A
    /// model whose calls never reported the cache-read field leaves the count
    /// at zero, so the row consults this before claiming a share. Defaults to
    /// all-unreported, so a payload that carries no flag for it reads as
    /// unknown rather than as a confirmed zero.
    #[serde(default)]
    pub provenance: UsageProvenance,
}

fn default_true() -> bool {
    true
}

/// The session-identity fields the /status command renders, wire form.
/// Mirrors the descriptor the composition root writes at session
/// creation. The TUI renders this directly; it never sees the descriptor
/// store trait. Provenance carries the session lineage (fresh / forked /
/// resumed-from-export) so the host surfaces where the session came from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDescriptorSummary {
    /// The user-set or auto-derived display name. None until a name is
    /// assigned.
    pub name: Option<String>,
    /// The original cwd the session started in.
    pub cwd: String,
    /// The model the session runs with.
    pub model: String,
    /// The build version that created the session (forward-compat signal).
    pub version: String,
    /// Where the session came from.
    pub provenance: SessionProvenance,
}

/// The provenance of a session, wire form. Fresh = minted new; ForkedFrom
/// = split off an existing session; ResumedFromExport = bootstrapped from
/// an exported transcript file; SpawnedBy = a sub-agent a parent runner
/// spawned.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SessionProvenance {
    #[default]
    Fresh,
    ForkedFrom {
        from_sid: String,
        from_seq: Option<u64>,
    },
    ResumedFromExport {
        source_session_id: String,
    },
    SpawnedBy {
        parent_session_id: String,
        subagent_type: String,
        task_id: String,
    },
}

#[cfg(test)]
mod tests;
