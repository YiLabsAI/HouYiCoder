//! Tests for this module.

use super::*;
use houyicoder_context::CheckpointId;

#[test]
fn test_reply_carries_outcome_fields() {
    let outcome = CompactionOutcome {
        made_progress: true,
        folded_count: 12,
        manifest_id: CheckpointId::new(),
        pre_compact_tokens: 8000,
        post_compact_tokens: 3000,
        recall_rate: Some(0.5),
        conflict_rate: None,
    };
    let reply = map_compact_reply(&outcome);
    assert!(reply.made_progress);
    assert_eq!(reply.folded_count, 12);
    assert_eq!(reply.pre_compact_tokens, Some(8000));
    assert_eq!(reply.post_compact_tokens, Some(3000));
    assert!(!reply.manifest_id.is_empty());
    assert_eq!(reply.recall_rate, Some(0.5));
}

#[test]
fn test_no_progress_reply_honest() {
    let outcome = CompactionOutcome {
        made_progress: false,
        folded_count: 0,
        manifest_id: CheckpointId::new(),
        pre_compact_tokens: 1000,
        post_compact_tokens: 1000,
        recall_rate: None,
        conflict_rate: None,
    };
    let reply = map_compact_reply(&outcome);
    assert!(!reply.made_progress);
    assert_eq!(reply.folded_count, 0);
    assert_eq!(reply.recall_rate, None);
}
