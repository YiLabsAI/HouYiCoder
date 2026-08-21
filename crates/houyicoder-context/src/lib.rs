//! The context layer: the ContextBackend storage
//! interface, the TurnEvent wire types, and context-window assembly.
//!
//! The append-only event log is the source of truth; compaction is
//! view-selection, not destruction — the raw log is never mutated. This crate
//! owns the storage INTERFACE (ContextBackend) and the wire types that cross
//! to backends. Backends (InMemory, LocalFile, sqlite, cloud) live in the
//! memory layer and impl ContextBackend — memory depends on this crate,
//! not the reverse (dependency inversion: the interface lives in the calling
//! layer).
//!
//! Disentangled from MemoryProvider (the semantic-recall interface, in the
//! memory layer): ContextStore keeps full-fidelity history; MemoryProvider
//! extracts facts. ContextStore makes MemoryProvider's lossy extraction safe
//! because the raw transcript is never destroyed.
//!
//! Engine-facing replay, hash-chain computation, and view assembly
//! (SessionStore wrapping a ContextBackend) live in the session layer.

// The modules are private and re-exported by name: the split between them is
// mechanical (it keeps each file under the size gate), so it must not harden
// into public contract, and a named list is what keeps a new pub item in a
// submodule from widening the API on its own.
mod hook_types;
mod memory_types;
mod sandbox_types;
pub use hook_types::{HookErrorKind, HookEventKind, HookVerdictKind};
pub use memory_types::{
    MemoryEntry, MemoryError, MemoryOrigin, MemoryRecallStats, MemoryScope, MemorySource,
    MemorySummary, memory_age_days, memory_age_label, memory_freshness_text, tokens_for,
};
pub use sandbox_types::{DirEntry, ExecConfig, ExecResult, SandboxError};

/// The storage interface: the ContextBackend trait and its error type.
mod backend;
pub use backend::{ContextBackend, ContextError, LenientRead, LogRangeRead, ReverseRead};

/// The session metadata sidecar (SessionMeta + SessionMetaStore trait). The
/// per-session descriptor written alongside the event log at <sid>/session.json.
mod meta;
pub use meta::{
    ContextMetaError, MetaUpdate, NameSource, SessionMeta, SessionMetaStore, SessionProvenance,
};

/// The compaction plan types (Disposition, TurnGroup, CheckpointManifest,
/// ContextSnapshot).
mod plan;
pub use plan::{CheckpointManifest, ContextSnapshot, Disposition, TurnGroup};

/// The identifier newtypes (session, event, checkpoint ids + the two hash
/// wrappers a logged event carries).
mod ids;
pub use ids::{BlockHash, CheckpointId, EventId, PrevHash, SessionId};

/// The append-only log's wire types (TurnEvent + the vocabulary of what a
/// record can be).
mod event;
pub use event::{PermissionVerdict, TruncationSignal, TurnEvent, TurnEventKind};
