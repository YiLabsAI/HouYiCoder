//! Map the durable session descriptor to its wire summary.

use houyicoder_protocol::frontend::status::{SessionDescriptorSummary, SessionProvenance};

/// Map the descriptor fields exposed to the frontend.
pub(crate) fn map_session_descriptor(
    descriptor: &houyicoder_context::SessionDescriptor,
) -> SessionDescriptorSummary {
    SessionDescriptorSummary {
        name: descriptor.name.clone(),
        cwd: descriptor.cwd.clone(),
        model: descriptor.model.clone(),
        version: descriptor.version.clone(),
        provenance: match &descriptor.provenance {
            houyicoder_context::SessionProvenance::Fresh => SessionProvenance::Fresh,
            houyicoder_context::SessionProvenance::ForkedFrom { from_sid, from_seq } => {
                SessionProvenance::ForkedFrom {
                    from_sid: from_sid.clone(),
                    from_seq: *from_seq,
                }
            }
            houyicoder_context::SessionProvenance::ResumedFromExport { source_session_id } => {
                SessionProvenance::ResumedFromExport {
                    source_session_id: source_session_id.clone(),
                }
            }
            houyicoder_context::SessionProvenance::SpawnedBy {
                parent_session_id,
                subagent_type,
                task_id,
            } => SessionProvenance::SpawnedBy {
                parent_session_id: parent_session_id.clone(),
                subagent_type: subagent_type.clone(),
                task_id: task_id.clone(),
            },
        },
    }
}

#[cfg(test)]
mod tests;
