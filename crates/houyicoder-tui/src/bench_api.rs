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
use crate::transcript::store::Transcript;
use crate::transcript::{SequencedFrame, TranscriptFrame};

/// Build a bare App carrying the given frame log. A bare App has no
/// server or runtime, which is all the rebuild and fold paths need. Takes
/// the log by value and returns the App, so the wrapper carries no broad
/// mutable-access parameter.
pub fn app_with_frames(frames: Vec<TranscriptFrame>) -> App {
    let mut app = composition::app();
    let sequenced: Vec<SequencedFrame> = frames.into_iter().map(Into::into).collect();
    app.sessions
        .current
        .transcript
        .with_frames_mut(|log| *log = sequenced);
    app
}

/// A frame log holding the given frames, for the append benchmark. The append
/// needs the log alone, so the wrapper returns it without the App around it:
/// an App parameter would be broad access the coupling ratchet must not carry.
pub fn transcript_with_frames(frames: Vec<TranscriptFrame>) -> Transcript {
    let mut transcript = Transcript::default();
    let sequenced: Vec<SequencedFrame> = frames.into_iter().map(Into::into).collect();
    transcript.with_frames_mut(|log| *log = sequenced);
    transcript
}

/// Append one frame to the resident log, the write the streaming path performs
/// per delta. The append benchmark names this instead of the log's own append,
/// so a change to how the log stores frames is absorbed here rather than in
/// every bench.
pub fn append_frame(transcript: &mut Transcript, frame: TranscriptFrame) {
    transcript.push_frame(frame);
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
        app.sessions.current.transcript.resident_bytes(),
        app.sessions.current.transcript.resident_byte_budget(),
    )
}

/// The absolute index of the oldest resident frame. A bench reads it beside
/// the byte total, because a payload-heavy log holds under the ceiling only by
/// draining the frames before this front.
pub fn resident_frame_front(app: &App) -> usize {
    app.sessions.current.transcript.frame_window_start()
}

/// Recompute the fold groups from the transcript lines and return the count.
/// The per-draw path reads the cache the rebuild maintains, so this full scan
/// measures the fold work now confined to the rebuild instead of repeated per
/// draw; the bench keeps it as the comparable fold-cost metric.
pub fn recompute_fold_group_count(app: &App) -> usize {
    compute_fold_groups(app.sessions.current.transcript.lines(), app.agent_busy()).len()
}

/// The fold groups the draw path reads: the cache the rebuild maintains.
/// Mirrors the main-view draw call, which borrows this slice rather than
/// rescanning, so the bench measures the cached read rather than the scan.
pub fn cached_fold_group_count(app: &App) -> usize {
    app.active_fold_groups().len()
}
