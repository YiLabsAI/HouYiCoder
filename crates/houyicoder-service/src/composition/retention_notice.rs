//! Retention startup notices: bad settings fields (surfaced here, not only
//! in the background sweep's tracing) + the count-cap backlog hint. Split
//! from composition.rs on the per-file size gate, same reasoning as the
//! startup-warnings module beside it.

/// Push the retention warnings + the backlog hint into the startup queue.
/// The warnings (a near-miss key, a bad type) must not wait for the sweep's
/// tracing to be seen; the backlog hint is the one signal the user gets
/// that the store is over the retention count, because the sweep skips
/// auto-apply above the threshold and nothing else tells the user. The
/// scan root is the store backend's own (reader and writer can never
/// disagree); None for an in-memory build, so the notice scans nothing.
pub(super) fn push_startup_notices(
    store_log_root: Option<std::path::PathBuf>,
    startup: &mut Vec<String>,
) {
    let (cfg, warnings) = houyicoder_config::retention::load_retention();
    startup.extend(
        warnings
            .iter()
            .map(|w| format!("{}: {}", w.field, w.reason)),
    );
    if cfg.session_retention_count > 0
        && let Some(root) = store_log_root
        && let Some(notice) =
            crate::session_prune::store_backlog_notice(&root, cfg.session_retention_count as usize)
    {
        startup.push(notice);
    }
}
