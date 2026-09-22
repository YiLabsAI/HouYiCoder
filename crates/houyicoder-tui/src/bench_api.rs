//! Benchmark surface for the transcript primitives. Gated by the bench
//! feature; the bench target declares required-features bench, so the
//! default build and the commit gate never compile these wrappers. They
//! expose the crate-internal entry points the benches need: building an
//! App over a frame log, the incremental rebuild, and the fold-group scan.
//! Each wrapper avoids a broad mutable-access parameter so the App
//! coupling ratchet is unaffected.

use crate::composition;
use crate::state::App;
use crate::transcript::{SequencedFrame, TranscriptFrame};

/// Build a bare App carrying the given frame log. A bare App has no
/// server or runtime, which is all the rebuild and fold paths need. Takes
/// the log by value and returns the App, so the wrapper carries no broad
/// mutable-access parameter.
pub fn app_with_frames(frames: Vec<TranscriptFrame>) -> App {
    let mut app = composition::app();
    let sequenced: Vec<SequencedFrame> = frames.into_iter().map(Into::into).collect();
    app.transcript.with_frames_mut(|log| *log = sequenced);
    app
}

/// Rebuild the transcript from the frame log. Takes the App by value and
/// returns it, so the wrapper carries no broad mutable-access parameter;
/// the bench builds a fresh App per iteration anyway.
pub fn rebuild_transcript(mut app: App) -> App {
    app.rebuild_transcript();
    app
}

/// The resident frame log's estimated byte total and its ceiling. A bench
/// asserts the log holds under the ceiling instead of growing with the corpus,
/// so the two values are read together.
pub fn resident_frame_bytes(app: &App) -> (u64, usize) {
    (
        app.transcript.resident_bytes(),
        app.transcript.resident_byte_budget(),
    )
}

/// The absolute index of the oldest resident frame. A bench reads it beside
/// the byte total, because a payload-heavy log holds under the ceiling only by
/// draining the frames before this front.
pub fn resident_frame_front(app: &App) -> usize {
    app.transcript.frame_window_start()
}

/// Recompute the fold groups from the transcript lines and return the count.
/// The per-draw path reads the cache the rebuild maintains, so this full scan
/// measures the fold work now confined to the rebuild instead of repeated per
/// draw; the bench keeps it as the comparable fold-cost metric.
pub fn recompute_fold_group_count(app: &App) -> usize {
    crate::fold::compute_fold_groups(app.transcript.lines(), app.agent_busy()).len()
}
