//! The session's injected persistence collaborators, injected by the
//! composition root when it assembles a session.

use std::sync::Arc;

use crate::resume_picker::SessionCatalog;
use crate::transcript::snapshot::TranscriptSnapshot;
use crate::view::export_log::ExportLog;
use crate::view::trajectory_pane::TrajectoryLog;

/// The session's injected persistence collaborators: the trajectory reader,
/// the transcript snapshot reader, the export writer, and the resumable-session
/// catalog, each facing the durable session log. The composition root injects
/// them together when it assembles a session; each is None in the stub and
/// bare paths, where the pane or command falls back to a mock reply or
/// reports the session as unavailable.
#[derive(Default)]
pub(crate) struct SessionPersistence {
    /// The trajectory reader, read by the trajectory pane and its keys.
    pub(crate) trajectory_log: Option<Arc<dyn TrajectoryLog>>,
    /// The export writer, driven by the export command to write a JSON dump.
    pub(crate) export_log: Option<Arc<dyn ExportLog>>,
    /// The transcript snapshot reader, read by the search view's whole-file path.
    pub(crate) snapshot: Option<Arc<dyn TranscriptSnapshot>>,
    /// The resumable-session catalog, read by the resume picker.
    pub(crate) catalog: Option<Arc<dyn SessionCatalog>>,
}
