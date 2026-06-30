//! The wire form of a runner status snapshot, returned to the frontend
//! over the wire so the TUI renders /status without importing the engine
//! crate. The engine StatusSnapshot carries a borrowed breaker_state label
//! and a Duration cool-down; the wire form owns both (a String and a
//! whole-second count) so it crosses the wire with no lifetime or
//! resilience-type leakage. Usage is the shared protocol llm type.
//!
//! The session-meta summary is an optional sidecar attached by the server
//! (the engine snapshot has no access to the sidecar store). The TUI
//! renders the /status identity fields (version, session name, cwd,
//! provenance) from it; None on the stub + test paths where no sidecar
//! exists.

use serde::{Deserialize, Serialize};

use crate::llm::Usage;

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
    /// Cumulative provider-reported usage across every turn this runner
    /// has driven since the accumulator was last reset.
    pub cumulative_usage: Usage,
    /// The input_tokens of the last response (a proxy for how full the
    /// model context window is right now).
    pub last_input_tokens: u32,
    /// The provider-reported context window, from ModelCapabilities.
    pub context_window: u32,
    /// Total tool executions across the session (success + error).
    pub tool_calls: u32,
    /// Tool executions that returned a value (not an error payload).
    pub tool_success: u32,
    /// Tool executions that errored (an error payload).
    pub tool_errors: u32,
    /// The session-metadata sidecar the server attaches so the TUI can
    /// render the identity fields (version / name / cwd / provenance)
    /// without importing the sidecar store. None on the stub path and
    /// when no sidecar exists yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<SessionMetaSummary>,
    /// Which env var provides the auth token (DASHSCOPE / OPENAI / HOUYICODER
    /// API KEY), or None. The server resolves this so the TUI never imports
    /// the config crate + never sees the secret value (only the source name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token_source: Option<String>,
    /// The provider base URL the server resolves (env or default). The TUI
    /// renders it without naming the config crate.
    #[serde(default)]
    pub base_url: String,
    /// Which settings sources contribute ("User", "User, Project", or
    /// "(defaults)"). Resolved server-side so the TUI does not touch the
    /// settings file.
    #[serde(default)]
    pub setting_sources: String,
    /// Whether turn-entry recall injection + the background extractor run.
    /// Read from the settings file server-side so the TUI never imports the
    /// config crate. Display-only: the user edits the settings file to flip it.
    #[serde(default = "default_true")]
    pub auto_memory: bool,
    /// Whether the background consolidation dream fires. Same provenance +
    /// display-only contract as auto_memory.
    #[serde(default = "default_true")]
    pub auto_dream: bool,
    /// Per-model token breakdown for the Usage sub-tab's "Usage by model"
    /// section. Sorted by input+output descending. Empty when the session
    /// has used a single model (the flat cumulative_usage already covers it)
    /// or no usage has been recorded. The server attaches this from the
    /// observability log's cost summary; the TUI renders it without
    /// importing the engine crate. Carries token counts only — no USD
    /// (no pricing source; see the observability design's cost decision).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_model: Vec<ModelUsageView>,
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
}

fn default_true() -> bool {
    true
}

/// The session-identity fields the /status command renders, wire form.
/// Mirrors the sidecar descriptor the composition root writes at session
/// creation. The TUI renders this directly; it never sees the sidecar
/// store trait. Provenance carries the session lineage (fresh / forked /
/// resumed-from-export) so the host surfaces where the session came from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetaSummary {
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
/// an exported transcript file.
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
}
