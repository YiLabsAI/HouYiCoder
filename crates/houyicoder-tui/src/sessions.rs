//! The frontend's session set: the one presented session and the resumable
//! sessions the resume picker reads. They live beside each other rather than
//! nested because the presented session is a single value while the catalog
//! spans every resumable session on disk.

use std::sync::Arc;

use crate::resume_picker::SessionCatalog;
use crate::session::Session;
use crate::session::persistence::SessionPersistence;

/// The host's session holdings. current is the presented session and is
/// always present; catalog is the durable resumable set the resume picker
/// lists, None in the bare path and when no catalog is wired. persistence
/// holds the current session's injected read ports against the durable log:
/// the trajectory reader, the export writer, and the transcript snapshot
/// reader. They are the current session's collaborators, so build_app
/// rebuilds them from the new bundle on every switch; the catalog, in
/// contrast, spans every resumable session rather than tracking the
/// current session's own collaborators.
pub(crate) struct Sessions {
    /// The presented session, replaced whole on a switch.
    pub(crate) current: Session,
    /// The resumable-session catalog the picker reads.
    pub(crate) catalog: Option<Arc<dyn SessionCatalog>>,
    /// The current session's injected persistence collaborators against the
    /// durable log: trajectory reader, export writer, transcript snapshot.
    pub(crate) persistence: SessionPersistence,
}

impl Sessions {
    /// A session set holding the given presented session, no catalog, and
    /// empty persistence collaborators. The composition root fills the
    /// persistence ports when it wires a live session.
    pub(crate) fn new(current: Session) -> Self {
        Self {
            current,
            catalog: None,
            persistence: SessionPersistence::default(),
        }
    }
}
