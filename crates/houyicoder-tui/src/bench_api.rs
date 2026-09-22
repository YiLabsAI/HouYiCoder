//! Benchmark surface for the transcript primitives. Gated by the bench
//! feature; the bench target declares required-features bench, so the
//! default build and the commit gate never compile these wrappers. They
//! expose the crate-internal entry points the benches need: building an
//! App over a frame log, the incremental rebuild, and the fold-group scan.
//! Each wrapper avoids a broad mutable-access parameter so the App
//! coupling ratchet is unaffected.

use crate::composition;
use crate::fold::compute_fold_groups;
use crate::state::App;
use crate::transcript::{SequencedFrame, TranscriptFrame};

/// Build a bare App carrying the given frame log. A bare App has no
/// server or runtime, which is all the rebuild and fold paths need. Takes
/// the log by value and returns the App, so the wrapper carries no broad
/// mutable-access parameter.
pub fn app_with_frames(frames: Vec<TranscriptFrame>) -> App {
    let mut app = composition::app();
    let sequenced: Vec<SequencedFrame> = frames.into_iter().map(Into::into).collect();
    *app.transcript.frames_mut() = sequenced;
    app
}

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
