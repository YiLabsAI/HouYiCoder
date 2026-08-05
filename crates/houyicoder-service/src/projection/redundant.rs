//! Project the engine redundant-call records to the wire form so the
//! /trajectory pane surfaces same-input re-issues as a self-evolution
//! reward signal. Split from projection.rs so that file stays under the
//! file-size gate.

use houyicoder_core::observability::evolution::{RedundancyKind, RedundantCall};
use houyicoder_protocol::frontend::trajectory::RedundantCallEntry;

pub(crate) fn project_redundant(records: &[RedundantCall]) -> Vec<RedundantCallEntry> {
    records
        .iter()
        .map(|r| RedundantCallEntry {
            tool: r.tool.clone(),
            input_preview: r.input_preview.clone(),
            kind: match r.kind {
                RedundancyKind::SameBatch => "same-batch".to_string(),
                RedundancyKind::CrossBatch => "cross-batch".to_string(),
            },
            gap: r.gap,
            last_seq: r.last_seq,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_core::observability::evolution::{RedundancyKind, RedundantCall};

    #[test]
    fn test_project_maps_kind_label() {
        let records = vec![
            RedundantCall {
                tool: "read".into(),
                input_preview: "{\"file_path\":\"a.rs\"}".into(),
                kind: RedundancyKind::SameBatch,
                gap: 0,
                last_seq: 7,
                prior_ref: None,
            },
            RedundantCall {
                tool: "bash".into(),
                input_preview: "git status".into(),
                kind: RedundancyKind::CrossBatch,
                gap: 12,
                last_seq: 30,
                prior_ref: None,
            },
        ];
        let wire = project_redundant(&records);
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0].kind, "same-batch");
        assert_eq!(wire[0].tool, "read");
        assert_eq!(wire[0].gap, 0);
        assert_eq!(wire[1].kind, "cross-batch");
        assert_eq!(wire[1].last_seq, 30);
    }

    #[test]
    fn test_project_redundant_empty() {
        assert!(project_redundant(&[]).is_empty());
    }
}
