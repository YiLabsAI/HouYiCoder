//! The session's injected persistence collaborators, held by the Sessions
//! owner beside the presented session.

use std::sync::Arc;

use crate::transcript::snapshot::TranscriptSnapshot;
use crate::view::export_log::ExportLog;
use crate::view::trajectory_pane::TrajectoryLog;

/// The session's injected persistence collaborators: the trajectory reader,
/// the transcript snapshot reader, and the export writer, each facing the
/// durable session log. The composition root injects them together when it
/// assembles a live session; each is None in the stub and bare paths, where
/// the pane or command falls back to a mock reply or reports the session as
/// unavailable. The owner lives on Sessions beside the presented session,
/// because these ports are the current session's collaborators and swap
/// together on a switch; the resumable-session catalog lives there too,
/// beside rather than inside the presented session.
#[derive(Default)]
pub(crate) struct SessionPersistence {
    /// The trajectory reader, read by the trajectory pane and its keys.
    pub(crate) trajectory_log: Option<Arc<dyn TrajectoryLog>>,
    /// The export writer, driven by the export command to write a JSON dump.
    pub(crate) export_log: Option<Arc<dyn ExportLog>>,
    /// The transcript snapshot reader, read by the search view's whole-file path.
    pub(crate) snapshot: Option<Arc<dyn TranscriptSnapshot>>,
}
