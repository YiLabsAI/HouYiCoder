//! Memory-list/show wire mirror. The /memory command asks the server for the
//! stored memories; these are the typed payloads the response carries so the
//! TUI renders the list (or one entry's body) without importing the engine
//! provider. The wire carries source as a lowercase label string (not the
//! enum) so the protocol crate stays free of the context types.

use serde::{Deserialize, Serialize};

/// Which background memory task produced a MemorySaved event. The host maps
/// this to a render verb (extract -> Saved, dream -> Improved), keeping the
/// wording out of the wire payload (a typed token, not a string, per the
/// type-first rule). Lives here so both the engine (its live event) and the
/// wire (the frontend event) share one definition — no second enum to drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemorySavedKind {
    /// The extractor wrote new memories (from the conversation or a forked
    /// extraction pass, including the main-agent saved-this-turn skipped
    /// path). Rendered as Saved.
    Extracted,
    /// The consolidation dream touched memories (added, merged, or deleted).
    /// Rendered as Improved.
    Consolidated,
}

/// One stored memory's frontmatter: key, one-line description, source label,
/// and modification time (seconds since the UNIX epoch). No body content —
/// the listing path reads no full bodies, so a /memory browse stays cheap
/// regardless of store size. The body is fetched on demand via MemoryShow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemorySummaryEntry {
    pub key: String,
    pub description: String,
    /// Lowercase source label: user / feedback / project / reference.
    pub source: String,
    /// Lowercase scope label: user / project / auto — which storage root the
    /// topic lives in. Drives the /memory pane scope filter (per-project vs
    /// global vs auto-extracted).
    pub scope: String,
    pub mtime_secs: u64,
}

/// The full body of one memory: key, content, source label, description, and
/// mtime. The response to a /memory <key> show request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryDetail {
    pub key: String,
    pub content: String,
    /// Lowercase source label: user / feedback / project / reference.
    pub source: String,
    pub description: String,
    pub mtime_secs: u64,
}

/// Which memory toggle a /memory toggle command flips. Serialized lowercase so
/// the wire form reads auto / dream (matches the in-pane labels the user sees).
/// The toggle flips one switch at a time; the response carries the full
/// snapshot so the pane re-renders both rows from one round-trip.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MemoryToggleWhich {
    Auto,
    Dream,
}

/// Snapshot of both memory toggles (auto-memory + auto-dream) returned on a
/// read or a flip so the /memory pane renders on/off rows without importing
/// the config crate. Both fields default to true; a flip round-trips the new
/// state back so the pane updates immediately.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToggleState {
    pub auto_memory: bool,
    pub auto_dream: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summary_round_trips() {
        let entry = MemorySummaryEntry {
            key: "build-gate".into(),
            description: "make check must stay green".into(),
            source: "project".into(),
            scope: "project".into(),
            mtime_secs: 0,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(json.contains("\"key\":\"build-gate\""), "{json}");
        assert!(json.contains("\"mtimeSecs\""), "camelCase: {json}");
        assert!(json.contains("\"scope\":\"project\""), "{json}");
        let back: MemorySummaryEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, entry);
    }

    #[test]
    fn test_entry_round_trips() {
        let entry = MemoryDetail {
            key: "k".into(),
            content: "body".into(),
            source: "user".into(),
            description: "d".into(),
            mtime_secs: 99,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(json.contains("\"mtimeSecs\":99"), "{json}");
        let back: MemoryDetail = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, entry);
    }

    #[test]
    fn test_toggle_which_serializes_lowercase() {
        for (which, expect) in [
            (MemoryToggleWhich::Auto, "\"auto\""),
            (MemoryToggleWhich::Dream, "\"dream\""),
        ] {
            let json = serde_json::to_string(&which).expect("serialize");
            assert_eq!(json, expect);
            let back: MemoryToggleWhich = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, which);
        }
    }

    #[test]
    fn test_toggle_state_round_trips() {
        let state = ToggleState {
            auto_memory: true,
            auto_dream: false,
        };
        let json = serde_json::to_string(&state).expect("serialize");
        assert!(json.contains("\"autoMemory\":true"), "{json}");
        assert!(json.contains("\"autoDream\":false"), "{json}");
        let back: ToggleState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);
    }
}
