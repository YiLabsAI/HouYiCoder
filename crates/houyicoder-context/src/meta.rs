//! Session metadata sidecar: the per-session descriptor written alongside
//! the event log at <sid>/session.json. The hash chain spans TurnEvents only,
//! so mutable session-level fields (name, cwd, model, provenance) live in a
//! separate sidecar rather than appended to the chain, so the chain
//! stays pure (mutable fields in the sidecar, immutable events in the chain).
//!
//! The store trait is in the interface layer (here) so the composition root
//! and the resume path name it without depending on a concrete disk impl. The
//! disk impl lives in the memory layer alongside the file backend; an
//! in-memory impl serves the test tier so unit tests never touch the real
//! home sessions dir. The trait is sync (not async like ContextBackend):
//! the sidecar is always a tiny local file, so a cloud-backed meta store is
//! not a design target, and a sync trait lets the composition root write the
//! initial sidecar without a runtime or a spawned thread.

use crate::SessionId;
use serde::{Deserialize, Serialize};

/// The provenance of a session: where it came from. Fresh = minted new;
/// ForkedFrom = --fork-session off an existing session; ResumedFromExport =
/// a one-time bootstrap from an exported transcript file. Recorded so /status
/// can show the lineage and a resume can carry the forked-from sid forward.
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
pub struct SessionMeta {
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
}

/// Read + write the per-session metadata sidecar. The trait is in the
/// interface layer so the composition root, the resume path, and the TUI
/// /rename command name a single store without taking a direct dep on the
/// concrete disk impl. The disk impl lives in the memory layer.
pub trait SessionMetaStore: Send + Sync {
    /// Read the sidecar for a session. None when no sidecar exists (a
    /// session created before this sidecar landed, or a fresh test store).
    fn read_meta(&self, session: SessionId) -> Option<SessionMeta>;

    /// Write (create or overwrite) the sidecar for a session. Used at
    /// creation + on /rename. Atomic write so a crash mid-write cannot
    /// leave a half-written sidecar.
    fn write_meta(&self, session: SessionId, meta: &SessionMeta) -> Result<(), ContextMetaError>;

    /// Read, edit, and write the sidecar as one indivisible step, returning
    /// whether there was a sidecar to edit.
    ///
    /// Every caller that changes one field must use this rather than
    /// read_meta followed by write_meta. Those two calls each write a whole
    /// sidecar derived from the state they read, so two of them interleaving
    /// leaves the later write erasing the earlier one's field: a rename and
    /// a model switch racing lose the name or the model, not a conflict but
    /// a silent revert. Implementations serialize the read and the write
    /// against their own concurrent calls. The scope of that guarantee is
    /// one store instance in one process; a second process writing the same
    /// sidecar is still last-writer-wins.
    ///
    /// The edit closure runs while the store holds its lock, so it must not
    /// call back into the store.
    fn update_meta(
        &self,
        session: SessionId,
        edit: &mut dyn FnMut(&mut SessionMeta),
    ) -> Result<MetaUpdate, ContextMetaError>;

    /// Delete the sidecar (session dir teardown). Best-effort: a missing
    /// sidecar is not an error.
    fn delete_meta(&self, session: SessionId);

    /// List all sessions with a sidecar, newest updated first. The picker
    /// uses this to populate the global session list.
    fn list_metas(&self) -> Vec<(SessionId, SessionMeta)>;
}

/// Whether an update_meta call found a sidecar to edit. Absent is not an
/// error: a session whose sidecar has not materialized yet is a normal
/// state (the sidecar is written on the first durable append), and callers
/// differ on whether it deserves a message - a rename reports it to the
/// user, a background model write ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaUpdate {
    /// The sidecar existed; the edit was applied and written back.
    Written,
    /// No sidecar for the session; nothing was edited or written.
    Absent,
}

/// A sidecar read/write failure. Distinct from ContextError (the event-log
/// error) so metadata I/O cannot be confused with chain corruption.
#[derive(Debug)]
pub struct ContextMetaError(pub String);

impl std::fmt::Display for ContextMetaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session meta error: {}", self.0)
    }
}

impl std::error::Error for ContextMetaError {}
