//! The compaction plan types: how a session's events sit in the active
//! context view. The plan is view-level — the raw log is never mutated,
//! only the served window moves. Split from lib.rs so the crate root
//! stays under the file-size gate.

use crate::{CheckpointId, EventId, SessionId, TurnEvent};
use serde::{Deserialize, Serialize};

/// How an event sits in the active context view. The plan is view-level: the
/// raw log is never mutated, only the served window moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Disposition {
    /// In the active view verbatim.
    Verbatim,
    /// Folded into the summary; raw event stays in the log, out of the view.
    Summarized,
    /// Moved to the CAS; the view carries a BlockRef instead of the body.
    Referenced,
}

/// An atomic span of events sharing one Disposition. The unit is the API
/// round (one assistant response and the events it owns): an assistant
/// message and its immediately-following tool-use blocks are always in the
/// same group, so a compaction plan cannot split thinking from its tool_use
/// (the API rejects a tool_use whose thinking block landed in a different
/// fate). A tool_result may sit in its own group when it carries an
/// independent CAS reference, which does not break the integral round.
/// Events not covered by a plan default to Verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnGroup {
    /// The assistant message id anchoring this round, or the first event's
    /// id when the group holds no assistant message (a bare tool_result or
    /// a user prompt leading the session).
    pub turn_id: EventId,
    /// The shared disposition for every event in this group.
    pub disposition: Disposition,
    /// Event ids in this group, in replay order.
    pub event_ids: Vec<EventId>,
}

/// A compaction plan: which turns are verbatim / summarized / referenced in
/// the active view, plus the summary text. The raw log is untouched; applying
/// the plan to a replay produces the served window. The plan is per turn
/// group (one disposition per atomic API round), not per event: a per-event
/// plan could express an illegal split of thinking from its tool_use, which
/// the API rejects — the group makes that split unexpressable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub id: CheckpointId,
    pub session: SessionId,
    /// The last event id this plan covers (plans are append-only too).
    pub last_event: EventId,
    /// Summary of the Summarized span; None before the first compaction.
    pub summary: Option<String>,
    /// Per-turn-group disposition, in replay order. Each group is an atomic
    /// API round; thinking and its tool_use always share a group.
    pub plan: Vec<TurnGroup>,
    pub ts: u64,
}

/// A snapshot of the served context window for a session. The events field
/// is the full replay (raw log is never mutated). When a checkpoint exists,
/// the manifest carries the per-turn-group Disposition plan; the caller
/// applies it to produce the served view (Verbatim kept, Summarized folded,
/// Referenced replaced). No manifest means full replay (no plan applied).
#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    pub session: SessionId,
    /// Events in the served window (full replay; the manifest projects them).
    pub events: Vec<TurnEvent>,
    /// The most recent checkpoint, if any compaction has run.
    pub last_checkpoint: Option<CheckpointId>,
    /// All checkpoint ids for the session, oldest first (rewind picker).
    pub rewind_points: Vec<CheckpointId>,
    /// The latest checkpoint manifest, if one exists. None when no compaction
    /// has run (full replay). The caller applies this to the events to produce
    /// the served view.
    pub manifest: Option<CheckpointManifest>,
}
