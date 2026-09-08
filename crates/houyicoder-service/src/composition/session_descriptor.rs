//! Construct and materialize session descriptors during assembly.

use super::*;
use houyicoder_context::{
    NameSource, SessionDescriptor, SessionDescriptorStore, SessionProvenance,
};

/// Build the descriptor for a fresh session.
pub(crate) fn build_initial_descriptor(model: &str, project: Option<&str>) -> SessionDescriptor {
    let cwd = workspace_cwd(project.map(str::to_string));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    SessionDescriptor {
        name: None,
        name_source: NameSource::Auto,
        cwd,
        model: model.to_string(),
        provenance: SessionProvenance::Fresh,
        version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: now,
        child_session_ids: Vec::new(),
    }
}

/// Materialize a fresh descriptor on the first durable append.
pub(crate) fn materialize_hook(
    descriptor_store: Arc<dyn SessionDescriptorStore>,
    initial_descriptor: SessionDescriptor,
) -> Arc<dyn Fn(SessionId) + Send + Sync> {
    Arc::new(move |sid| {
        if let Err(e) = descriptor_store.write_descriptor(sid, &initial_descriptor) {
            tracing::warn!("session descriptor: materialize failed: {e}");
        }
    })
}
