//! Session descriptor written alongside the event log at
//! <sid>/session.json. The hash chain spans SessionLogEntries only, so mutable
//! session-level fields live in a separate descriptor while immutable events stay
//! in the chain.
//!
//! The store trait is in the interface layer so composition and resume paths
//! can name it without depending on a concrete disk implementation. The disk
//! implementation lives in the memory layer alongside the file backend; an
//! in-memory implementation serves tests. The trait is synchronous because the
//! descriptor is always a tiny local file and a cloud-backed descriptor store is
//! not a design target.

use crate::SessionId;
use crate::ids::EventId;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::{Display, Formatter};

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

/// How the session name was set. Auto = derived from the session's first
/// prompt (the prompt is stored, the display name is derived from it);
/// User = set via /rename (stored, wins over auto). The source lets /rename
/// mark its write so a later auto-derivation does not clobber a user name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameSource {
    Auto,
    User,
}

/// One user input that states what the work is, together with the log event
/// it was read out of. The event id keeps the copy attributable: a later
/// reader can look the event up instead of trusting text with no origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRecord {
    /// The prompt in display form: control sequences removed, whitespace
    /// collapsed, capped to a title-sized length.
    pub text: String,
    /// The event this text was read from.
    pub event_id: EventId,
}

/// How much of a session's log the first and last prompt were read from. A
/// descriptor written before the fields existed has no coverage recorded, and
/// one whose log was only partly scanned cannot claim the two are the whole
/// history: a missing prompt looks absent when it was merely not read.
///
/// The claim covers observation, not capture: a prompt is written after its
/// event landed, and that write can fail or be cut short by a shutdown. A
/// reader that needs certainty about an absent prompt reads the log rather
/// than trusting this field alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCoverage {
    /// The log was never scanned end to end; an absent prompt means unknown,
    /// not missing.
    #[default]
    Unknown,
    /// Only the head and tail of the log were read.
    Partial,
    /// Every event was observed, either as it was appended or by a full scan.
    Complete,
}

/// The per-session descriptor. Written at session creation; updated on
/// /rename and on each prompt. The restore path and the status view read it.
/// Fields the engine needs to restore a session across process restart: cwd
/// (where to land), model (which provider config), provenance (lineage), name
/// (display + search), and the first and last prompt that state the work.
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
    /// spawn lands. serde default keeps descriptors written before this field
    /// readable because an old descriptor simply has no children.
    #[serde(default)]
    pub child_session_ids: Vec<String>,
    /// The first user input that states the work, recorded once the session
    /// is observed from its start. None on a descriptor written before the
    /// field existed, or before any task statement arrived.
    #[serde(default)]
    pub first_prompt: Option<PromptRecord>,
    /// The newest user input that states the work, recorded so a reader learns
    /// a session's current subject without opening its log.
    #[serde(default)]
    pub last_prompt: Option<PromptRecord>,
    /// How much of the log the first and last prompt were read from.
    #[serde(default)]
    pub prompt_coverage: PromptCoverage,
}

impl SessionDescriptor {
    /// The descriptor a delegated child is recorded with: the delegation that
    /// started it, and the second it was created. The constructor leaves the
    /// working directory and model empty because the caller knows only the
    /// delegation. A store that materializes a descriptor on a session's first
    /// append writes its own initial fields first, and the delegation boundary
    /// then records the provenance on top, so a child in production carries
    /// the assembly's working directory and model. When the store materializes
    /// none, this shape is what a resume by id reads. No listing path offers a
    /// child: the picker, the continue fallback, and the counts all filter to
    /// user sessions.
    ///
    /// Three paths build a child's descriptor with this constructor: the
    /// delegation boundary, when the store had no descriptor to edit; the
    /// pass that repairs one whose boundary write was lost; and a resume that
    /// finds a child log with no record. They know different amounts: the
    /// boundary starts a child whose runner observes every later append, so a
    /// coverage claim taken there stays true, while the repair pass and the
    /// resume run after the child may have written, and a log neither reads
    /// cannot be claimed as covered.
    pub fn delegated_child(
        provenance: SessionProvenance,
        created_at_secs: u64,
        coverage: PromptCoverage,
    ) -> Self {
        Self {
            name: None,
            name_source: NameSource::Auto,
            cwd: String::new(),
            model: String::new(),
            provenance,
            version: env!("CARGO_PKG_VERSION").to_string(),
            created_at: created_at_secs,
            child_session_ids: Vec::new(),
            first_prompt: None,
            last_prompt: None,
            prompt_coverage: coverage,
        }
    }
}

/// Read and write the per-session descriptor. The trait is in the
/// interface layer so composition, resume, and TUI rename paths share one
/// store without depending on the concrete disk implementation.
pub trait SessionDescriptorStore: Send + Sync {
    /// Read the descriptor for a session. None when no descriptor exists.
    fn read_descriptor(&self, session: SessionId) -> Option<SessionDescriptor>;

    /// Write or overwrite a descriptor atomically so a crash cannot leave a
    /// partial descriptor.
    fn write_descriptor(
        &self,
        session: SessionId,
        descriptor: &SessionDescriptor,
    ) -> Result<(), SessionDescriptorError>;

    /// Write the descriptor only when the session has none, as one indivisible
    /// step, and report whether this call wrote it. A writer that must not
    /// overwrite a record someone else landed uses this rather than
    /// read_descriptor followed by write_descriptor: those two calls leave a
    /// window for exactly the record the caller set out to keep. Like
    /// update_descriptor, the serialization covers one store instance in one
    /// process; a second process, or a second store over the same root,
    /// remains unguarded, and what this closes is the window between the two
    /// calls.
    fn write_descriptor_if_absent(
        &self,
        session: SessionId,
        descriptor: &SessionDescriptor,
    ) -> Result<bool, SessionDescriptorError>;

    /// Read, edit, and write the descriptor as one indivisible step, reporting
    /// which of three outcomes happened: the store wrote the edited record, the
    /// edit left the record as it found it so nothing was written, or the
    /// session held no record to edit.
    ///
    /// Every caller that changes one field must use this rather than
    /// read_descriptor followed by write_descriptor. Those calls each write a
    /// whole descriptor derived from the state they read, so interleaving callers
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

    /// Delete the descriptor. A missing descriptor is not an error.
    fn delete_descriptor(&self, session: SessionId);
}

/// What an update_descriptor call did with the descriptor. Absent is not an
/// error because a descriptor materializes on the first durable append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptorUpdate {
    /// The descriptor existed; the edit was applied and written back.
    Written,
    /// The descriptor existed and the edit left it exactly as it was, so
    /// nothing was written. A record that already holds what an edit would set
    /// must not pay a write: an edit can run on every launch, and rewriting an
    /// unchanged record replaces its file for no reader.
    Unchanged,
    /// No descriptor for the session; nothing was edited or written.
    Absent,
}

/// A descriptor read or write failure, distinct from event-log errors.
#[derive(Debug)]
pub struct SessionDescriptorError(pub String);

impl Display for SessionDescriptorError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "session descriptor error: {}", self.0)
    }
}

impl Error for SessionDescriptorError {}

#[cfg(test)]
mod tests;
