//! Map engine redundant-call records to the wire form for the /trajectory
//! pane's self-evolution reward signal.

use houyicoder_core::observability::evolution::{RedundancyKind, RedundantCall};
use houyicoder_protocol::frontend::trajectory::RedundantCallEntry;

pub(crate) fn map_redundant_entries(records: &[RedundantCall]) -> Vec<RedundantCallEntry> {
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
mod tests;
