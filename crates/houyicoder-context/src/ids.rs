//! The identifier newtypes of the context layer: the session, event, and
//! checkpoint ids, plus the two hash wrappers that appear on a logged event.
//!
//! Split from the crate root on size grounds. They belong together: each is
//! a thin newtype over a generated id with the same mint, Display, and
//! parse-back trio, and every one of them crosses the wire, so their serde
//! shape is a compatibility surface that is easier to review in one file
//! than scattered through the event types that carry them.

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
