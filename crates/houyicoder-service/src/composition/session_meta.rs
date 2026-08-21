//! Session metadata sidecar wiring for the composition root.
//!
//! Split out of the composition module on size grounds (the same pattern as
//! the memory + worktree submodules): the composition file is the sole
//! assembly site and an acknowledged churn magnet, so a whole concern moves
//! out rather than trimming prose. The function here writes the initial
//! <sid>/session.json at creation; nothing outside the composition root
//! consumes it.

use super::*;
use houyicoder_context::{NameSource, SessionMeta, SessionMetaStore, SessionProvenance};

/// Build the initial session.json meta for a fresh session. name starts
/// None (auto-derived from the first prompt at display time); /rename later
/// sets it + flips name_source to User. cwd is the resolved workspace so
/// resume lands where the session started; provenance is Fresh (the resume +
/// fork paths overwrite this before assembling). Pure: the caller decides
/// WHEN to write - eagerly for an in-memory build (no disk, no orphan) or
/// on the first durable append for a disk build (so a build that never runs
/// a turn leaves no empty session dir).
pub(crate) fn build_initial_meta(model: &str, project: Option<&str>) -> SessionMeta {
    let cwd = workspace_cwd(project.map(str::to_string));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    SessionMeta {
        name: None,
        name_source: NameSource::Auto,
        cwd,
        model: model.to_string(),
        provenance: SessionProvenance::Fresh,
        version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: now,
    }
}

/// The first-durable hook that materializes a fresh session's sidecar.
/// Captures the meta store + the initial meta built at construction; fires
/// once per session on the first durable append, so a disk build that never
/// runs a turn leaves no dir. Best-effort: a write failure logs and the
/// engine keeps running (resume degrades, the turn does not).
pub(crate) fn materialize_hook(
    meta_store: Arc<dyn SessionMetaStore>,
    initial_meta: SessionMeta,
) -> Arc<dyn Fn(SessionId) + Send + Sync> {
    Arc::new(move |sid| {
        if let Err(e) = meta_store.write_meta(sid, &initial_meta) {
            tracing::warn!("session meta: materialize failed: {e}");
        }
    })
}
