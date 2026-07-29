//! Fold state for one streaming turn: the running assistant text, reasoning
//! chunks, tool calls, usage, and the provider's finish_reason, accumulated
//! as the stream's deltas land. The drive loop folds events into this state
//! and builds the authoritative CompletionResponse from it once the stream
//! ends. Split from call.rs so the fold state stays a self-contained concern
//! separate from the turn-driving orchestration.

use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};

/// The accumulator the drive loop folds streaming events into. The fields are
/// pub(crate) so call.rs's fold_event mutates them inline as deltas land; the
/// response is built once at stream end via into_response. finish_reason drives
/// the recovery decision ("length" = cut at the output cap → resume-direct
/// nudge before surfacing a truncation notice).
#[derive(Default, Clone)]
pub(crate) struct StreamFold {
    pub(crate) assistant_text: String,
    pub(crate) reasoning: Vec<String>,
    pub(crate) tool_calls: Vec<OutputItem>,
    pub(crate) usage: Usage,
    /// The provider's finish_reason for the stream (stop / length / ...).
    pub(crate) finish_reason: Option<String>,
}

impl StreamFold {
    /// Build the authoritative CompletionResponse from the folded state. The
    /// loop appends it (deltas + this AssistantMessage + ToolCalls) to the log.
    pub(crate) fn into_response(self, model: String) -> CompletionResponse {
        let mut output: Vec<OutputItem> = Vec::new();
        // Join the per-delta reasoning chunks into ONE Reasoning output item.
        // The fold pushes each ReasoningDelta's text onto state.reasoning; if
        // we mapped each element to its own OutputItem the transcript would
        // render one thinking row per delta (a word-chunked wall of rows).
        if !self.reasoning.is_empty() {
            output.push(OutputItem::Reasoning {
                text: self.reasoning.join(""),
            });
        }
        if !self.assistant_text.is_empty() {
            output.push(OutputItem::Text {
                text: self.assistant_text,
            });
        }
        output.extend(self.tool_calls);
        CompletionResponse {
            output,
            usage: self.usage,
            model,
        }
    }
}
