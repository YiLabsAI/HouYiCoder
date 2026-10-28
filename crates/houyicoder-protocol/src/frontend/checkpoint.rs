//! The views the sandbox pane reads: what the host can prove about the fences
//! a session runs inside, and what recovery can still restore.
//!
//! Nothing here is inferred at the terminal. A fact the host did not measure
//! stays absent, and a write the host could not judge is listed as unreadable
//! rather than guessed at.

use serde::{Deserialize, Serialize};

/// The kernel fence's state, as the host last measured it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FenceState {
    /// The kernel refuses writes outside the allowed roots.
    Enforced,
    /// The session runs without a kernel fence.
    NotEnforced,
    /// A fence was asked for and could not be set up; the reason is carried so
    /// the pane states it instead of reporting a healthy boundary.
    Failed(String),
    /// The platform offers no fence.
    Unavailable,
}

/// What the host can prove about the boundaries a session runs inside.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxBoundariesView {
    /// The fence's state.
    pub fence: FenceState,
    /// The workspace root the session was assembled over.
    pub workspace_root: String,
    /// Every directory a write may land in: the workspace root plus the ones
    /// the user added beyond it.
    pub writable_roots: Vec<String>,
}

/// What a recorded write's target holds now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckpointState {
    /// The target holds the bytes the write left.
    Applied,
    /// The target holds the bytes the write found.
    NotApplied,
    /// Something touched the target after the write: it holds neither side, or
    /// it is a different file under other permissions.
    Conflict,
    /// The earlier bytes were never kept, so the target cannot be matched
    /// against them.
    Unverified,
}

/// One side of a recorded write, as the record kept it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointImage {
    /// Whether the file was there at all.
    pub present: bool,
    /// The name the stored bytes are kept under. Absent when they were not
    /// kept.
    pub hash: Option<String>,
    /// How many bytes the file held.
    pub bytes: Option<u64>,
}

/// One recorded file write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointView {
    /// The name the write recorded itself under; a restore names this.
    pub operation: String,
    /// The workspace path the write targeted.
    pub target: String,
    /// What the target holds now.
    pub state: CheckpointState,
    /// When the write committed, in milliseconds since the epoch. Absent for
    /// a write that never landed.
    pub written_ms: Option<u64>,
    /// What the target held before the write.
    pub before: CheckpointImage,
    /// What the write left.
    pub after: CheckpointImage,
}

/// A recorded write the host could not judge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadableView {
    /// The name the write recorded itself under, when its directory name could
    /// be read as text.
    pub operation: Option<String>,
    /// Why the host could not judge it.
    pub reason: String,
}

/// Every write a session recorded, split by whether the host could judge it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointListView {
    /// The writes the host judged, in the order they were recorded.
    pub checkpoints: Vec<CheckpointView>,
    /// The writes the host could not judge, and why.
    pub unreadable: Vec<UnreadableView>,
}

/// What a restore did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestoreView {
    /// The target already held the earlier bytes.
    Unchanged,
    /// The earlier bytes are back in the target.
    Restored,
    /// The target changed after the write, so the earlier bytes went to a copy
    /// beside it. No copy when the write created the file.
    Kept { copy: Option<String> },
    /// The earlier bytes were never kept, so the write cannot be undone.
    Unbacked,
}

/// The reply to a restore.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRestoreView {
    /// The write that was restored.
    pub operation: String,
    /// What the restore did.
    pub outcome: RestoreView,
}
