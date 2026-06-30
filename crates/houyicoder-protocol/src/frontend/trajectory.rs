//! The wire form of a trajectory audit-log entry, returned by the /trajectory
//! query so the TUI renders the append-only event log without importing the
//! engine or context crate. The engine TurnEvent carries the event id, the
//! wall-clock ts, the prev_hash linking it into the chain, and the kind; the
//! wire form mirrors exactly those audit fields as owned, serde-friendly
//! values (a string kind label, a u64 ts, a string event id, a hex prev_hash).
//!
//! This is distinct from the live SessionUpdate stream: the stream is the
//! chat render surface (user / agent / thought / tool chunks), while the
//! trajectory is the durable audit record (one row per event, with the
//! hash chain a client can verify the server is not dropping events). Kinds
//! the base session/update has no standard variant for (permission decision,
//! compaction boundary, summary, meta user) ride here too, so the audit log
//! is complete the stream + the acpx notifications are projections of the
//! same events for different surfaces.

use serde::{Deserialize, Serialize};

/// The /trajectory response: the audit-log entries (the existing 3-level
/// drill-down) + the redundant-call observations (a self-evolution reward
/// signal section). Split so the pane renders both from one query round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryResponse {
    pub entries: Vec<TrajectoryEntry>,
    #[serde(default)]
    pub redundant: Vec<RedundantCallEntry>,
    /// Number of events the current binary did not recognize (serde(other)
    /// fallback on TurnEventKind). These are events from a newer binary
    /// or a corrupt line; they appear as "unknown" in the entries list but
    /// this count surfaces them as a visible warning so a stale build or
    /// a corrupt log does not silently drop events.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unknown_count: u32,
}

/// One entry in the audit-log trajectory, wire form. The durable per-event
/// record the server projects from the engine turn-event log. The TUI
/// renders /trajectory from this without importing the engine crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryEntry {
    /// The event kind label (user / assistant / tool_call / verdict / ...),
    /// fixed-width at the render boundary, not here.
    pub kind: String,
    /// The wall-clock ts (epoch seconds, matches the engine TurnEvent.ts).
    pub ts: u64,
    /// The event id (string form of the engine EventId).
    pub event_id: String,
    /// The prev_hash linking this event into the append-only chain, as a hex
    /// string. None for the genesis event (the first event in the chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_hash: Option<String>,
    /// Wall-clock duration of a tool call in ms. None for non-tool events;
    /// Some(ms) for ToolResult so the trajectory surface can flag slow calls
    /// without re-deriving timing from the raw event log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// One redundant tool-call observation, wire form (mirrors the engine
/// RedundantCall). The /trajectory pane surfaces these as a "redundant
/// calls" section so the model's same-input re-issues are visible as a
/// self-evolution reward signal. The kind label is a stable machine string
/// (same-batch / cross-batch); the pane renders a human name from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedundantCallEntry {
    /// The tool name (read, bash, ...).
    pub tool: String,
    /// Canonical input preview (capped).
    pub input_preview: String,
    /// same-batch (two identical calls in one message) or cross-batch
    /// (a later call repeats a prior with no intervening write).
    pub kind: String,
    /// Calls between the prior same-input call and this one (0 for same-batch).
    pub gap: u64,
    /// The engine seq of the prior call this one duplicates (for a future
    /// click-to-jump correlation; not yet wired to a trajectory event range).
    pub last_seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> TrajectoryEntry {
        TrajectoryEntry {
            kind: "assistant".into(),
            ts: 1_700_000_000,
            event_id: "01HXYZ".into(),
            prev_hash: Some("0123456789abcdef".into()),
            duration_ms: None,
        }
    }

    #[test]
    fn test_round_trips() {
        let original = fixture();
        let json = serde_json::to_string(&original).expect("serialize");
        let back: TrajectoryEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, original);
    }

    #[test]
    fn test_uses_camel_case_keys() {
        let json = serde_json::to_string(&fixture()).expect("serialize");
        assert!(json.contains("\"eventId\""), "camelCase: {json}");
        assert!(json.contains("\"prevHash\""), "camelCase: {json}");
        assert!(!json.contains("event_id"), "snake leaked: {json}");
    }

    #[test]
    fn test_genesis_entry_omits_hash() {
        let mut e = fixture();
        e.prev_hash = None;
        let json = serde_json::to_string(&e).expect("serialize");
        assert!(!json.contains("prevHash"), "None skipped: {json}");
        let back: TrajectoryEntry = serde_json::from_str(&json).expect("deserialize");
        assert!(back.prev_hash.is_none());
    }
}
