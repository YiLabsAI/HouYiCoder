//! Benchmark surface for the transcript primitives. Gated by the bench
//! feature; the bench target declares required-features bench, so the
//! default build and the commit gate never compile these wrappers. They
//! expose the two crate-internal entry points the benches need: the
//! incremental rebuild and the fold-group scan. Both wrappers avoid a
//! broad mutable-access parameter so the App coupling ratchet is unaffected.

use crate::fold::compute_fold_groups;
use crate::state::App;

/// Rebuild the transcript from the frame log. Takes the App by value and
/// returns it, so the wrapper carries no broad mutable-access parameter;
/// the bench builds a fresh App per iteration anyway.
pub fn rebuild_transcript(mut app: App) -> App {
    app.rebuild_transcript();
    app
}

/// Count the fold groups the scanner builds for the current transcript.
/// Returns a count because the group type stays crate-internal; the work
/// is the scan, not the returned type.
pub fn fold_group_count(app: &App, agent_busy: bool) -> usize {
    compute_fold_groups(&app.transcript, agent_busy).len()
}
