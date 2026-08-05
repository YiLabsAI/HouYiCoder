//! Project the engine compaction outcome to the wire reply so the TUI
//! renders /compact without importing the engine or context crate.

use houyicoder_core::agent::compact::CompactOutcome;
use houyicoder_protocol::frontend::compact::CompactReply;

/// Project the CompactOutcome (made progress, folded count, manifest id,
/// pre/post token estimates) to the wire reply.
pub(crate) fn project_compact_reply(outcome: &CompactOutcome) -> CompactReply {
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
mod tests {
    use super::*;
    use houyicoder_context::CheckpointId;
    use houyicoder_core::agent::compact::CompactOutcome;

    #[test]
    fn test_reply_carries_outcome_fields() {
        let outcome = CompactOutcome {
            made_progress: true,
            folded_count: 12,
            manifest_id: CheckpointId::new(),
            pre_compact_tokens: 8000,
            post_compact_tokens: 3000,
            recall_rate: Some(0.5),
            conflict_rate: None,
        };
        let reply = project_compact_reply(&outcome);
        assert!(reply.made_progress);
        assert_eq!(reply.folded_count, 12);
        assert_eq!(reply.pre_compact_tokens, Some(8000));
        assert_eq!(reply.post_compact_tokens, Some(3000));
        assert!(!reply.manifest_id.is_empty());
        assert_eq!(reply.recall_rate, Some(0.5));
    }

    #[test]
    fn test_no_progress_reply_honest() {
        let outcome = CompactOutcome {
            made_progress: false,
            folded_count: 0,
            manifest_id: CheckpointId::new(),
            pre_compact_tokens: 1000,
            post_compact_tokens: 1000,
            recall_rate: None,
            conflict_rate: None,
        };
        let reply = project_compact_reply(&outcome);
        assert!(!reply.made_progress);
        assert_eq!(reply.folded_count, 0);
        assert_eq!(reply.recall_rate, None);
    }
}
