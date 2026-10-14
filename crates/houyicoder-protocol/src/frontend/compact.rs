//! Wire result for manual session compaction.

use serde::{Deserialize, Serialize};

/// Manual compaction outcome exposed to protocol clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactReply {
    /// True when at least one event was Summarized (the window shrank). False
    /// means the manifest is all-Verbatim and compacting again would not help.
    pub made_progress: bool,
    /// Number of events folded into the summary (Summarized disposition).
    pub folded_count: u64,
    /// The persisted checkpoint manifest id (a /rewind target).
    pub manifest_id: String,
    /// Selected transcript estimate before compaction. None when the
    /// engine did not measure (the engine currently always provides a value).
    pub pre_compact_tokens: Option<u64>,
    /// Selected transcript estimate after compaction. None when the
    /// engine did not measure (the engine currently always provides a value).
    pub post_compact_tokens: Option<u64>,
    /// Recall rate since the previous compaction: conversation_search matches
    /// that landed in the folded span, divided by this compaction's folded
    /// count. None when no recall was measured. An instrumentation signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_rate: Option<f64>,
    /// Conflict rate: file paths the LLM summary fabricated (mentioned a
    /// touched file that was never touched) over the backbone's ground-truth
    /// file set. None when no summary was produced. v1's free measurement
    /// (the v2 signal for shrinking the LLM path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_rate: Option<f64>,
}

impl CompactReply {
    /// Build a reply from the engine outcome's scalar fields.
    pub fn new(
        made_progress: bool,
        folded_count: u64,
        manifest_id: impl Into<String>,
        pre_compact_tokens: Option<u64>,
        post_compact_tokens: Option<u64>,
    ) -> Self {
        Self {
            made_progress,
            folded_count,
            manifest_id: manifest_id.into(),
            pre_compact_tokens,
            post_compact_tokens,
            recall_rate: None,
            conflict_rate: None,
        }
    }

    /// Attach a recall rate to the reply (the adapter maps the engine
    /// outcome's recall_rate through).
    pub fn with_recall_rate(mut self, rate: Option<f64>) -> Self {
        self.recall_rate = rate;
        self
    }

    /// Attach a conflict rate to the reply (the adapter maps the engine
    /// outcome's conflict_rate through).
    pub fn with_conflict_rate(mut self, rate: Option<f64>) -> Self {
        self.conflict_rate = rate;
        self
    }
}

#[cfg(test)]
mod tests;
