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

/// Write the initial session.json at session creation. name starts None
/// (auto-derived from the first prompt at display time); /rename later sets
/// it + flips name_source to User. cwd is the resolved workspace so resume
/// lands where the session started; provenance is Fresh (the resume + fork
/// paths overwrite this before assembling). Best-effort: a sidecar write
/// failure must not brick startup -- the engine runs without it, /status
/// just shows less. The error is surfaced on stderr so it is not silent.
pub(crate) fn write_initial_session_meta(
    meta_store: &Arc<dyn SessionMetaStore>,
    session: SessionId,
    model: &str,
    project: Option<&str>,
) {
    let cwd = workspace_cwd(project.map(str::to_string));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let meta = SessionMeta {
        name: None,
        name_source: NameSource::Auto,
        cwd,
        model: model.to_string(),
        provenance: SessionProvenance::Fresh,
        version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: now,
    };
    if let Err(e) = meta_store.write_meta(session, &meta) {
        tracing::warn!("session meta: write failed: {e}; /status will show less");
    }
}
