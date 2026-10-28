//! Persist the frozen retention decisions when a turn changed them, so a
//! reopened session can send the prefix bytes its previous process sent.

use houyicoder_context::SessionEvent;
use houyicoder_context::SessionId;

use crate::agent::RunError;
use crate::agent::Runner;
use crate::agent::append::new_event;

impl Runner {
    /// Append the decision map when a decision write changed it. A steady
    /// turn writes nothing, so the log grows only when the retention answer
    /// for a block actually changed.
    pub(super) async fn append_cache_decisions(
        &self,
        session: SessionId,
        model: &str,
    ) -> Result<(), RunError> {
        let Some(decisions) = self.cached_prefix.drain_if_dirty() else {
            return Ok(());
        };
        self.store
            .append(new_event(
                session,
                SessionEvent::CacheDecisions {
                    decisions,
                    model: model.to_string(),
                },
            ))
            .await?;
        Ok(())
    }
}
