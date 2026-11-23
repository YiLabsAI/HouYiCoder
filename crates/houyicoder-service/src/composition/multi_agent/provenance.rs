//! Child provenance: the descriptor a delegated child carries so the resume
//! picker can tell it from a session the user started.

use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use houyicoder_context::{
    DescriptorUpdate, PromptCoverage, SessionDescriptor, SessionDescriptorStore, SessionId,
    SessionProvenance,
};

/// Stamp SpawnedBy provenance on a child session's descriptor. Called at the
/// spawn boundary, before the child runs, so a live child is never read as one
/// of the user's own sessions. The delegation record the child opens with is
/// its first durable append, which materializes the descriptor on a store that
/// materializes one, so the update normally edits a file already there.
/// Best-effort: a stamp failure logs and continues (the child still ran).
pub(super) fn record_provenance(
    descriptor_store: &Option<Arc<dyn SessionDescriptorStore>>,
    child_sid: SessionId,
    parent_sid: SessionId,
    subagent_type: &str,
    task_id: &str,
) {
    if let Some(store) = descriptor_store {
        let parent_str = parent_sid.to_string();
        let prov = SessionProvenance::SpawnedBy {
            parent_session_id: parent_str,
            subagent_type: subagent_type.to_string(),
            task_id: task_id.to_string(),
        };
        let outcome = store.update_descriptor(child_sid, &mut |descriptor| {
            descriptor.provenance = prov.clone();
        });
        // Written means there was a descriptor to edit, and Unchanged means the
        // one there already states this provenance. Anything else -- a store
        // that materializes none on a first append, or an update that failed
        // -- leaves the provenance unrecorded, so create a descriptor here.
        // Complete is truthful at this point: the child has written nothing
        // yet and its runner observes every later append.
        if !matches!(
            outcome,
            Ok(DescriptorUpdate::Written | DescriptorUpdate::Unchanged)
        ) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let descriptor =
                SessionDescriptor::delegated_child(prov, now, PromptCoverage::Complete);
            if let Err(e) = store.write_descriptor(child_sid, &descriptor) {
                tracing::warn!("child provenance write failed: {e}");
            }
        }
    }
}
