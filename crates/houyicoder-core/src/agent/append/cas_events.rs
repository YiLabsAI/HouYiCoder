//! Persist what the session observed about the block store: the forms each
//! assembly carried. A read is written by the reader that issued it, because
//! only that reader knows which session asked.
//!
//! An assembly record lands after the turn's usage, so a reader that stops at
//! a terminal sees the turn's cost before its block views.

use houyicoder_context::{SessionEvent, SessionId};

use crate::agent::{RunError, Runner, append::new_event};

impl Runner {
    /// Append one record per assembly that carried a stored block, and nothing
    /// for an assembly that carried none.
    pub(super) async fn append_cas_views(&self, session: SessionId) -> Result<(), RunError> {
        let estimate_unit = self.cas_views.estimate_unit().to_string();
        for (assembly_id, views) in self.cas_views.take() {
            self.store
                .append(new_event(
                    session,
                    SessionEvent::CasModelView {
                        assembly_id,
                        views,
                        estimate_unit: estimate_unit.clone(),
                    },
                ))
                .await?;
        }
        Ok(())
    }
}
