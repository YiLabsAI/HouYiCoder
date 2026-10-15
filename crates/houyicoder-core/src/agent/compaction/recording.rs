//! Durable recording of compaction manifests and events.
//!
//! The manifest is written before CompactionBoundary and Summary. These
//! operations are not transactional; partial failure can leave an unused
//! manifest that a later compaction supersedes.

use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    CheckpointManifest, ContextError, Disposition, EventId, SessionEvent, SessionId,
    SessionLogEntry,
};
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Result of a compaction recording. Carries the persisted manifest, whether
/// any events were actually folded (no-progress detection), and a count of
/// folded turns.
#[derive(Debug, Clone)]
pub struct RecordedCompaction {
    /// The persisted manifest (also written to the backend).
    pub manifest: CheckpointManifest,
    /// Number of events folded into the summary (Summarized disposition).
    pub folded_count: usize,
    /// True when at least one event was Summarized (progress was made).
    pub made_progress: bool,
}

/// Persist the manifest and its boundary events. The token counts bracket the
/// fold and are carried on the boundary so a reader sees what it reclaimed
/// without re-measuring the span.
pub(super) async fn record_compaction(
    store: &dyn SessionLog,
    session: SessionId,
    manifest: &CheckpointManifest,
    pre_tokens: u64,
    post_tokens: u64,
) -> Result<RecordedCompaction, ContextError> {
    let folded_count = manifest
        .plan
        .iter()
        .filter(|g| g.disposition == Disposition::Summarized)
        .map(|g| g.event_ids.len())
        .sum::<usize>();
    let made_progress = folded_count > 0;
    store.write_checkpoint(manifest.clone()).await?;
    if let Some(summary_text) = &manifest.summary {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        store
            .append(SessionLogEntry {
                id: EventId::new(),
                session,
                ts: now,
                prev_hash: None,
                event: SessionEvent::CompactionBoundary {
                    checkpoint: manifest.id,
                    pre_tokens,
                    post_tokens,
                },
            })
            .await?;
        store
            .append(SessionLogEntry {
                id: EventId::new(),
                session,
                ts: now,
                prev_hash: None,
                event: SessionEvent::Summary {
                    text: summary_text.clone(),
                },
            })
            .await?;
    }
    Ok(RecordedCompaction {
        manifest: manifest.clone(),
        folded_count,
        made_progress,
    })
}

/// Pre-flight compress threshold: the context window minus the room the model
/// needs to respond (capped so a huge max_output does not erase the buffer)
/// minus an estimation margin for tiktoken drift on non-tiktoken-native
/// models. Saturates to 0 for windows smaller than the reserve (tiny-window
/// stubs trip compress immediately, which is correct — there is no room to
/// serve anything).
///
/// An absolute buffer beats a 95% ratio: on a 200k window the ratio left
/// only 10k headroom, too thin for a model that needs 8-16k to respond. The
/// absolute reserves real output room and scales correctly to 1M-class
/// windows (the buffer derives from the resolved window; a 1M-capable model
/// whose limit is mis-resolved to 200k still anchors a 200k buffer — that is
/// a window-resolution gap, not a miscompute in this formula).
pub(crate) fn pre_flight_threshold(window: u32, max_output_tokens: u32) -> u32 {
    const MAX_OUTPUT_RESERVE_CAP: u32 = 20_000;
    const ESTIMATION_MARGIN: u32 = 13_000;
    let reserve = max_output_tokens.min(MAX_OUTPUT_RESERVE_CAP) + ESTIMATION_MARGIN;
    window.saturating_sub(reserve)
}

#[cfg(test)]
mod tests;
