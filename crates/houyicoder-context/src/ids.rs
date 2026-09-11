//! Stable identifier and hash newtypes used by context records.

use serde::{Deserialize, Serialize};
use ulid::Ulid;
use uuid::Uuid;

/// A session id. UUID v4 (hyphenated). Not monotonic (unlike EventId) -- the ordering
/// invariant lives in the per-session event log, not the id, so a
/// collision-resistant random id is the right shape. sid-keyed layout
/// (<sid>/log.jsonl) uses the hyphenated Display form as the dir segment.
/// Deserialize is tolerant of a legacy ULID string (pre-change exports) so
/// an old session log resumes after the sid-format change; the ULID's 128
/// bits are reinterpreted as a Uuid. Serialize is always the hyphenated
/// UUID form (forward format only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    /// Mint a fresh session id (UUID v4).
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl SessionId {
    /// Parse a display string back into a SessionId. Accepts a hyphenated
    /// UUID (the forward format) or a legacy ULID (pre-change exports);
    /// both are 128 bits, so the ULID is reinterpreted as a Uuid. Used by
    /// the resume path to rehydrate a session from its sid.
    pub fn from_display_string(s: &str) -> Option<Self> {
        if let Ok(u) = s.parse::<Uuid>() {
            return Some(SessionId(u));
        }
        s.parse::<Ulid>()
            .ok()
            .map(|u| SessionId(Uuid::from_u128(u.into())))
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        SessionId::from_display_string(&s).ok_or_else(|| {
            serde::de::Error::custom(format!("session id is neither a UUID nor a ULID: {s}"))
        })
    }
}

/// An event id. ULID-backed; monotonic within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(Ulid);

impl EventId {
    /// Mint a fresh event id.
    pub fn new() -> Self {
        Self(Ulid::generate())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl EventId {
    /// Parse a display string (ULID) back into an EventId.
    pub fn from_display_string(s: &str) -> Option<Self> {
        s.parse::<Ulid>().ok().map(Self)
    }
}

/// The unique identity of one memory-change notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryChangeId(Ulid);

impl MemoryChangeId {
    /// Mint a fresh memory-change identity.
    pub fn new() -> Self {
        Self(Ulid::generate())
    }

    /// Parse a display string back into a MemoryChangeId.
    pub fn from_display_string(s: &str) -> Option<Self> {
        s.parse::<Ulid>().ok().map(Self)
    }
}

impl Default for MemoryChangeId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for MemoryChangeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod memory_change_id_tests {
    use super::*;

    #[test]
    fn test_memory_id_round_trips() {
        let id = MemoryChangeId::default();
        assert_ne!(id, MemoryChangeId::new());
        let rendered = id.to_string();
        assert_eq!(MemoryChangeId::from_display_string(&rendered), Some(id));
        let json = serde_json::to_string(&id).expect("serialize memory change id");
        assert_eq!(
            serde_json::from_str::<MemoryChangeId>(&json).expect("deserialize memory change id"),
            id
        );
    }

    #[test]
    fn test_memory_id_rejects_invalid() {
        assert!(MemoryChangeId::from_display_string("not-an-id").is_none());
    }
}

/// A checkpoint id (a compaction plan + summary snapshot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CheckpointId(Ulid);

impl CheckpointId {
    /// Mint a fresh checkpoint id.
    pub fn new() -> Self {
        Self(Ulid::generate())
    }
}

impl Default for CheckpointId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl CheckpointId {
    /// Parse a display string (ULID) back into a CheckpointId.
    pub fn from_display_string(s: &str) -> Option<Self> {
        s.parse::<Ulid>().ok().map(Self)
    }
}

/// A content-addressed block hash (SHA-256, hex). CAS dedup keys large tool
/// outputs / file blobs out of the in-context view while keeping them
/// retrievable. The interface is defined here; v0 backends return Unsupported.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlockHash(pub String);

/// A 32-byte hash linking one event to the previous (tamper-evidence spine).
/// None on the first event of a session. The caller (SessionStore) computes
/// the chain; the backend stores it verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrevHash(pub [u8; 32]);

/// A typed agent identity for the NextStep::Handoff variant and the
/// multi-agent handoff surface. A name string today; the multi-agent
/// runtime gives this a real registry and capability set. Sibling to the
/// spawn-port AgentIdentity type, which stays a distinct type (not merged).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);
