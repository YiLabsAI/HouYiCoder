//! Session descriptor sidecar written alongside the event log at
//! <sid>/session.json. The hash chain spans SessionLogEntries only, so mutable
//! session-level fields live in a separate sidecar while immutable events stay
//! in the chain.
//!
//! The store trait is in the interface layer so composition and resume paths
//! can name it without depending on a concrete disk implementation. The disk
//! implementation lives in the memory layer alongside the file backend; an
//! in-memory implementation serves tests. The trait is synchronous because the
//! sidecar is always a tiny local file and a cloud-backed descriptor store is
//! not a design target.

use crate::SessionId;
use serde::{Deserialize, Serialize};

/// The provenance of a session: where it came from. Fresh = minted new;
/// ForkedFrom = --fork-session off an existing session; ResumedFromExport =
/// a one-time bootstrap from an exported transcript file; SpawnedBy = a
/// sub-agent a parent runner spawned. Recorded so /status can show the
/// lineage and a resume can carry the forked-from sid forward.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionProvenance {
    Fresh,
    ForkedFrom {
        from_sid: String,
        from_seq: Option<u64>,
    },
    ResumedFromExport {
        source_session_id: String,
    },
    /// A sub-agent a parent runner spawned. Carries the parent's sid, the
    /// agent type, and the task id so the parent can correlate the child's
    /// result back to the spawn that produced it.
    SpawnedBy {
        parent_session_id: String,
        subagent_type: String,
        task_id: String,
    },
}

/// How the session name was set. Auto = derived from the first prompt (the
/// picker computes it on the fly, so it is NOT stored -- only the source is);
/// User = set via /rename (stored, wins over auto). This lets /rename mark
/// its write so a later auto-derivation does not clobber a user name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameSource {
    Auto,
    User,
}

/// The per-session descriptor. Written at session creation; updated on
/// /rename. Read by the resume path + /status. Fields the engine needs to
/// restore a session across process restart: cwd (where to land), model
/// (which provider config), provenance (lineage), name (display + search).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDescriptor {
    /// User-set name. None = derive from the first prompt at display time.
    pub name: Option<String>,
    /// Whether name is user-set (wins) or auto-derived.
    pub name_source: NameSource,
    /// The original cwd the session started in. Resume falls back to the
    /// current cwd if this path is gone, with a warning.
    pub cwd: String,
    /// The model the session ran with. Resume restores it; if unavailable
    /// the current config is used with a warning.
    pub model: String,
    /// Where this session came from.
    pub provenance: SessionProvenance,
    /// The houyi version that created the session (forward-compat signal).
    pub version: String,
    /// Unix-epoch seconds at creation.
    pub created_at: u64,
    /// The child sessions this session spawned, in spawn order. Empty until a
    /// spawn lands. serde default keeps sidecars written before this field
    /// readable because an old sidecar simply has no children.
    #[serde(default)]
    pub child_session_ids: Vec<String>,
}

/// Read and write the per-session descriptor sidecar. The trait is in the
/// interface layer so composition, resume, and TUI rename paths share one
/// store without depending on the concrete disk implementation.
pub trait SessionDescriptorStore: Send + Sync {
    /// Read the sidecar for a session. None when no sidecar exists.
    fn read_descriptor(&self, session: SessionId) -> Option<SessionDescriptor>;

    /// Write or overwrite a sidecar atomically so a crash cannot leave a
    /// partial descriptor.
    fn write_descriptor(
        &self,
        session: SessionId,
        descriptor: &SessionDescriptor,
    ) -> Result<(), SessionDescriptorError>;

    /// Read, edit, and write the sidecar as one indivisible step, returning
    /// whether there was a sidecar to edit.
    ///
    /// Every caller that changes one field must use this rather than
    /// read_descriptor followed by write_descriptor. Those calls each write a
    /// whole sidecar derived from the state they read, so interleaving callers
    /// can silently revert each other's fields. Implementations serialize the
    /// read and write against their own concurrent calls. The guarantee covers
    /// one store instance in one process; a second process remains
    /// last-writer-wins.
    ///
    /// The edit closure runs while the store holds its lock, so it must not
    /// call back into the store.
    fn update_descriptor(
        &self,
        session: SessionId,
        edit: &mut dyn FnMut(&mut SessionDescriptor),
    ) -> Result<DescriptorUpdate, SessionDescriptorError>;

    /// Delete the sidecar. A missing sidecar is not an error.
    fn delete_descriptor(&self, session: SessionId);

    /// List all sessions with a sidecar.
    fn list_descriptors(&self) -> Vec<(SessionId, SessionDescriptor)>;
}

/// Whether an update_descriptor call found a sidecar to edit. Absent is not
/// an error because a descriptor materializes on the first durable append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptorUpdate {
    /// The sidecar existed; the edit was applied and written back.
    Written,
    /// No sidecar for the session; nothing was edited or written.
    Absent,
}

/// A descriptor sidecar read or write failure, distinct from event-log errors.
#[derive(Debug)]
pub struct SessionDescriptorError(pub String);

impl std::fmt::Display for SessionDescriptorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session descriptor error: {}", self.0)
    }
}

impl std::error::Error for SessionDescriptorError {}

#[cfg(test)]
#[path = "descriptor_tests.rs"]
mod descriptor_tests;
