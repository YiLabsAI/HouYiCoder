//! Lineage repair for the sessions store: write the descriptor a delegated
//! child should have had when the boundary could not. The child's own log
//! already names its parent, so its class is right either way; the descriptor
//! is where every other reader expects the lineage, and a store that carries
//! it needs no log opened. Only a child whose log opens with the delegation
//! is reachable here; an older one is left to the store cleanup.

use std::path::Path;

use houyicoder_context::session_class::{ParentLink, scan_sessions};
use houyicoder_context::{
    SessionDescriptor, SessionDescriptorError, SessionDescriptorStore, SessionId, SessionProvenance,
};
use houyicoder_memory::FileDescriptorStore;

/// What one pass wrote, and what it could not write. A failure is counted
/// rather than raised: one unwritable descriptor does not stop the pass, and a
/// silent half-run would leave the residue unexplained.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DescriptorRepair {
    pub written: usize,
    pub failed: usize,
}

/// Give every delegated child with no descriptor in this store the one its log
/// implies. The store is built here, at the root it was handed: a caller
/// names the store to repair and cannot root it elsewhere by accident, so a
/// descriptor lands in the child's own directory beside its log.
pub fn repair_child_descriptors(root: &Path) -> DescriptorRepair {
    let store = FileDescriptorStore::new(root.to_path_buf());
    repair_child_descriptors_in(root, &store)
}

/// The pass itself, over a store the caller built. Only a directory whose
/// log names a parent and which has no descriptor beside it is touched -- a
/// child whose boundary write was lost; a record the store already holds is
/// kept, not overwritten.
pub(crate) fn repair_child_descriptors_in(
    root: &Path,
    store: &dyn SessionDescriptorStore,
) -> DescriptorRepair {
    let mut repair = DescriptorRepair::default();
    for entry in scan_sessions(root) {
        // A link is carried exactly where the descriptor left the lineage open
        // and the log answered it, which is the residue this pass exists for.
        let Some(link) = entry.parent.as_ref() else {
            continue;
        };
        match write_child_descriptor_if_absent(store, entry.sid, link) {
            Ok(true) => repair.written += 1,
            Ok(false) => {}
            Err(e) => {
                repair.failed += 1;
                tracing::warn!("child descriptor repair failed for {}: {e}", entry.sid);
            }
        }
    }
    repair
}

/// Whether the child's descriptor was written. False when one is already
/// there: the store checks under its own lock, so a record that landed
/// before this call is kept. The lock's scope is the trait's, not wider.
fn write_child_descriptor_if_absent(
    store: &dyn SessionDescriptorStore,
    session: SessionId,
    link: &ParentLink,
) -> Result<bool, SessionDescriptorError> {
    let descriptor = SessionDescriptor::delegated_child(
        SessionProvenance::SpawnedBy {
            parent_session_id: link.parent_session_id.clone(),
            subagent_type: link.subagent_type.clone(),
            task_id: session.to_string(),
        },
        link.created_at_secs,
    );
    store.write_descriptor_if_absent(session, &descriptor)
}

#[cfg(test)]
#[path = "session_repair_tests.rs"]
mod tests;
