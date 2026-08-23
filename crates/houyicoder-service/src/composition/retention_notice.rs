//! Retention startup notices: bad settings fields (surfaced here, not only
//! in the background sweep's tracing) + the count-cap backlog hint. Split
//! from composition.rs on the per-file size gate, same reasoning as the
//! startup-warnings module beside it.

use houyicoder_context::SessionId;

/// Push the retention warnings + the backlog hint into the startup queue.
/// The warnings (a near-miss key, a bad type) must not wait for the sweep's
/// tracing to be seen; the backlog hint is the one signal the user gets that
/// the store is over the retention count, because the sweep skips
/// auto-apply above the threshold and nothing else tells the user. The scan
/// root is the store backend's own (reader and writer can never disagree);
/// None for an in-memory build, so the notice scans nothing.
pub(super) fn push_startup_notices(
    store_log_root: Option<std::path::PathBuf>,
    current_session: SessionId,
    startup: &mut Vec<String>,
) {
    let (cfg, warnings) = houyicoder_config::retention::load_retention();
    startup.extend(
        warnings
            .iter()
            .map(|w| format!("{}: {}", w.field, w.reason)),
    );
    // The gap-range precise plan uses a policy built from the same settings
    // the sweep reads, so a TTL or count change applies to both. The
    // protected set carries only the current session (no lock-held scan):
    // the gap notice carries no prunable number, so an approximate
    // protected set cannot drift against cleanup's authoritative plan.
    let policy = crate::session_prune::PrunePolicy {
        ttl_secs: (cfg.session_retention_days as u64) * 24 * 3600,
        empty_ttl_secs: crate::session_prune::EMPTY_TTL_SECS,
        max_count: cfg.session_retention_count as usize,
        protected: vec![current_session],
        snapshot_ttl_secs: 0,
        debug_max_bytes: 0,
    };
    if let Some(root) = store_log_root
        && let Some(notice) = crate::session_prune::store_backlog_notice(
            &root,
            cfg.session_retention_count as usize,
            cfg.prune_confirm_threshold as usize,
            &policy,
        )
    {
        startup.push(notice);
    }
}
