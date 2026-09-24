//! Memory-list/show wire mirror. The /memory command asks the server for the
//! stored memories; these are the typed payloads the response carries so the
//! TUI renders the list (or one entry's body) without importing the engine
//! provider. The wire carries source as a lowercase label string (not the
//! enum) so the protocol crate stays free of the context types.

use serde::{Deserialize, Serialize};

/// Wire identity for one emitted set of memory changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryChangeId(pub String);

/// The producer responsible for memory changes emitted together.
///
/// Forward-compatible: a future producer may emit an origin this enum does
/// not yet name. An unrecognized tag deserializes to Unknown so the event
/// survives rather than being dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryChangeOrigin {
    /// A save_memory call made by the primary agent.
    PrimaryAgent,
    /// Automatic extraction after a run.
    AutoMemory,
    /// Automatic memory consolidation.
    AutoDream,
    /// A producer the receiver does not yet name. Only produced by
    /// deserialization of an unrecognized tag; the producer never emits it.
    #[serde(other)]
    Unknown,
}

/// The operation applied to one memory key.
///
/// Forward-compatible: a future producer may emit an operation this enum
/// does not yet name. An unrecognized tag deserializes to Unknown rather
/// than failing the whole event, so the notice still lands with its key
/// and the unknown operation is shown rather than dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryOperation {
    /// A memory was stored for a key with no prior record.
    Created,
    /// A memory was stored for a key whose prior record differed.
    Updated,
    /// A memory was deleted.
    Deleted,
    /// A memory moved to a broader scope.
    Promoted,
    /// A memory moved to a narrower scope.
    Demoted,
    /// An operation the receiver does not yet name. Only produced by
    /// deserialization of an unrecognized tag; the producer never emits it.
    #[serde(other)]
    Unknown,
}

/// One successful memory operation projected onto the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryChange {
    /// The exact memory key affected.
    pub key: String,
    /// The operation applied to the key.
    pub operation: MemoryOperation,
}

/// Which turn a memory change belongs to, so a notice that lands after the
/// user has already moved on can say so instead of reading as the turn on
/// screen.
///
/// Forward-compatible: a future producer may classify a change under a
/// causality this enum does not yet name. An unrecognized tag deserializes
/// to Unknown so the event survives rather than being dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryChangeCausality {
    /// The change belongs to the turn that just completed.
    ThisTurn,
    /// The change does not belong to the turn now on screen, either because
    /// the session holds a newer user input or because the frontier could
    /// not be read.
    PreviousTurn,
    /// A causality the receiver does not yet name. Only produced by
    /// deserialization of an unrecognized tag; the producer never emits it.
    #[serde(other)]
    Unknown,
}

/// An absent tag reads as an earlier turn: a frame from a producer that
/// predates the field must still deliver its changes, and claiming the turn
/// now on screen would be the stronger claim of the two.
impl Default for MemoryChangeCausality {
    fn default() -> Self {
        Self::PreviousTurn
    }
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
    fn test_causality_serializes_kebab_case() {
        for (value, expect) in [
            (MemoryChangeCausality::ThisTurn, "\"this-turn\""),
            (MemoryChangeCausality::PreviousTurn, "\"previous-turn\""),
            (MemoryChangeCausality::Unknown, "\"unknown\""),
        ] {
            let json = serde_json::to_string(&value).expect("serialize");
            assert_eq!(json, expect);
            let back: MemoryChangeCausality = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, value);
        }
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
