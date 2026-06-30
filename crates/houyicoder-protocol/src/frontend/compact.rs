//! The wire form of a manual /compact result, returned to the frontend so
//! the TUI renders the outcome (events folded, manifest id, pre/post token
//! estimates) without importing the engine or context crate. Mirrors the
//! engine CompressResult + the pre/post token counts the compaction path
//! captures around the summarizer call.

use serde::{Deserialize, Serialize};

/// The outcome of a manual /compact over the wire. The frontend renders a
/// one-line summary (progress made, events folded, pre-to-post token drop)
/// and surfaces the manifest id so a later /rewind can target the checkpoint.
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
    /// Token estimate of the served window before compaction. None when the
    /// estimate was unavailable (no tokenizer, empty session).
    pub pre_compact_tokens: Option<u64>,
    /// Token estimate of the served window after compaction (verbatim tail +
    /// summary). None when unavailable.
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

    /// Attach a recall rate to the reply (the projection maps the engine
    /// outcome's recall_rate through).
    pub fn with_recall_rate(mut self, rate: Option<f64>) -> Self {
        self.recall_rate = rate;
        self
    }

    /// Attach a conflict rate to the reply (the projection maps the engine
    /// outcome's conflict_rate through).
    pub fn with_conflict_rate(mut self, rate: Option<f64>) -> Self {
        self.conflict_rate = rate;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reply_round_trips() {
        let reply = CompactReply::new(true, 12, "ckpt_abc", Some(8000), Some(3000));
        let json = serde_json::to_string(&reply).expect("serialize");
        // camelCase wire fields, not snake_case.
        assert!(json.contains("madeProgress"), "camelCase in {json}");
        assert!(json.contains("foldedCount"), "camelCase in {json}");
        assert!(json.contains("manifestId"), "camelCase in {json}");
        assert!(json.contains("preCompactTokens"), "camelCase in {json}");
        assert!(json.contains("postCompactTokens"), "camelCase in {json}");
        let back: CompactReply = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, reply);
    }

    #[test]
    fn test_no_progress_round_trips() {
        // A no-progress reply (all-Verbatim manifest) still carries the
        // manifest id + zero folded + the pre/post estimates so the user
        // sees compaction was a no-op, not a silent failure.
        let reply = CompactReply::new(false, 0, "ckpt_empty", Some(1000), Some(1000));
        let json = serde_json::to_string(&reply).expect("serialize");
        let back: CompactReply = serde_json::from_str(&json).expect("deserialize");
        assert!(!back.made_progress);
        assert_eq!(back.folded_count, 0);
    }
}
