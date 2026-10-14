//! Map the engine compaction outcome to the wire reply.

use houyicoder_core::agent::CompactionOutcome;
use houyicoder_protocol::frontend::compact::CompactReply;

/// Map CompactionOutcome (progress, folded count, manifest id, token estimates)
/// to the wire reply.
pub(crate) fn map_compact_reply(outcome: &CompactionOutcome) -> CompactReply {
    CompactReply::new(
        outcome.made_progress,
        outcome.folded_count as u64,
        outcome.manifest_id.to_string(),
        Some(outcome.pre_compact_tokens),
        Some(outcome.post_compact_tokens),
    )
    .with_recall_rate(outcome.recall_rate)
    .with_conflict_rate(outcome.conflict_rate)
}

#[cfg(test)]
mod tests;
