//! The frontend's session set: the one presented session and the resumable
//! sessions the resume picker reads. They live beside each other rather than
//! nested because the presented session is a single value while the catalog
//! spans every resumable session on disk.

use std::sync::Arc;

use crate::resume_picker::SessionCatalog;
use crate::session::Session;

/// The host's session holdings. current is the presented session and is
/// always present; catalog is the durable resumable set the resume picker
/// lists, None in the bare path and when no catalog is wired.
pub(crate) struct Sessions {
    /// The presented session, replaced whole on a switch.
    pub(crate) current: Session,
    /// The resumable-session catalog the picker reads.
    pub(crate) catalog: Option<Arc<dyn SessionCatalog>>,
}

impl Sessions {
    /// A session set holding the given presented session and no catalog.
    pub(crate) fn new(current: Session) -> Self {
        Self {
            current,
            catalog: None,
        }
    }
}
