//! Where a session's task statement is written, and the writes still running.
//!
//! The recorder owns the one descriptor store a runner records through, so
//! every writer of that session shares it: the read-modify-write lock lives in
//! the store, and a second instance over the same root would let an
//! interleaved rename and prompt write revert each other's field.

use std::sync::Arc;

use houyicoder_context::session_intent::apply_prompt;
use houyicoder_context::{
    DescriptorUpdate, EventId, PromptRecord, SessionDescriptorStore, SessionId,
};
use tokio::task::spawn_blocking;

use super::PendingPromptWrites;

/// Records the task statement carried by each durable user input on the
/// session descriptor.
pub(crate) struct PromptRecorder {
    store: Arc<dyn SessionDescriptorStore>,
    pending: Arc<PendingPromptWrites>,
}

impl PromptRecorder {
    /// Record through this store from now on.
    pub(crate) fn new(store: Arc<dyn SessionDescriptorStore>) -> Self {
        Self {
            store,
            pending: Arc::new(PendingPromptWrites::default()),
        }
    }

    /// The store shared with the rest of the session's writers.
    pub(crate) fn store(&self) -> Arc<dyn SessionDescriptorStore> {
        Arc::clone(&self.store)
    }

    /// Fold a durable task statement into the descriptor's prompt fields.
    /// Runs after the append succeeded, so the event id names an event that
    /// exists. Best-effort: the descriptor is display metadata, and a failed
    /// write must not fail the turn.
    ///
    /// The write is a file write and an fsync, so it runs on a blocking
    /// worker, and it is not awaited: the turn must reach its first model call
    /// without paying that cost. Prompt order does not depend on which write
    /// lands first, because the descriptor keeps the prompt with the newest
    /// event id.
    pub(crate) fn record(&self, session: SessionId, prompt: Option<String>, event_id: EventId) {
        let Some(text) = prompt else {
            return;
        };
        let store = Arc::clone(&self.store);
        let record = PromptRecord { text, event_id };
        // The write counts as running from here, not from the moment the
        // blocking worker picks it up: a reader that arrives in between must
        // still wait for it. The ticket rides into the worker and drops when
        // the write returns or panics, so the count cannot be stranded.
        let ticket = self.pending.begin();
        let write = spawn_blocking(move || {
            let _ticket = ticket;
            let outcome = store.update_descriptor(session, &mut |descriptor| {
                apply_prompt(descriptor, record.clone());
            });
            // Reported on the worker that ran the write, so a shutdown that
            // drops the awaiting task does not take the diagnostic with it.
            match outcome {
                Ok(DescriptorUpdate::Written | DescriptorUpdate::Unchanged) => {}
                Ok(DescriptorUpdate::Absent) => {
                    tracing::warn!(
                        session = %session,
                        "session intent not recorded: no descriptor"
                    );
                }
                Err(e) => {
                    tracing::warn!(session = %session, "session intent not recorded: {e}");
                }
            }
        });
        // The handle is kept only to report a worker that died without
        // reporting; the turn does not wait for it.
        tokio::spawn(async move {
            if let Err(e) = write.await {
                tracing::warn!(
                    session = %session,
                    "session intent write ended without reporting: {e}"
                );
            }
        });
    }

    /// Wait until no prompt write is running.
    pub(crate) async fn wait(&self) {
        self.pending.wait().await;
    }
}
