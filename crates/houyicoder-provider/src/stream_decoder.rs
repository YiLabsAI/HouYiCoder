//! State tracking for an in-flight OpenAI-compatible SSE completion stream.

use houyicoder_protocol::llm::{LlmEvent, Usage};
use serde_json::Value;

use crate::openai_compat::{
    ToolCallAccum, abnormal_close_events, accumulate_tool_call, finalize_tool_calls,
};
use crate::usage::parse_usage;

/// Progress outcome after consuming one parsed JSON SSE chunk.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChunkAction {
    /// Continue reading more SSE chunks.
    Continue,
    /// Stop consuming chunks; the completion is complete.
    BreakStream,
}

/// Accumulates streaming state across incoming SSE chunks.
#[derive(Default)]
pub(crate) struct StreamDecoder {
    pub(crate) final_usage: Option<Usage>,
    pub(crate) finish_reason: Option<String>,
    pub(crate) tool_calls: Vec<ToolCallAccum>,
    pub(crate) text_started: bool,
    pub(crate) reasoning_started: bool,
}

impl StreamDecoder {
    /// Ingest a single parsed data-line JSON object. Appends any new protocol
    /// events to out and tells the caller whether to continue or break.
    pub(crate) fn handle_chunk(&mut self, json: &Value, out: &mut Vec<LlmEvent>) -> ChunkAction {
        if let Some(usage) = json.get("usage").filter(|v| !v.is_null()) {
            self.final_usage = Some(parse_usage(Some(usage)));
        }

        if self.finish_reason.is_some() {
            return if self.final_usage.is_some() {
                ChunkAction::BreakStream
            } else {
                ChunkAction::Continue
            };
        }

        let Some(choice) = json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
        else {
            return ChunkAction::Continue;
        };

        if let Some(delta) = choice.get("delta") {
            self.handle_delta(delta, out);
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.close_in_flight(out);
            for ev in finalize_tool_calls(std::mem::take(&mut self.tool_calls)) {
                out.push(ev);
            }
            self.finish_reason = Some(reason.to_string());
            if self.final_usage.is_some() {
                return ChunkAction::BreakStream;
            }
        }

        ChunkAction::Continue
    }

    fn handle_delta(&mut self, delta: &Value, out: &mut Vec<LlmEvent>) {
        if let Some(reasoning) = delta
            .get("reasoning_content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if !self.reasoning_started {
                self.reasoning_started = true;
                out.push(LlmEvent::ReasoningStart {
                    id: "reason-0".into(),
                });
            }
            out.push(LlmEvent::ReasoningDelta {
                id: "reason-0".into(),
                text: reasoning.into(),
            });
        }

        if let Some(content) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if self.reasoning_started {
                self.reasoning_started = false;
                out.push(LlmEvent::ReasoningEnd {
                    id: "reason-0".into(),
                });
            }
            if !self.text_started {
                self.text_started = true;
                out.push(LlmEvent::TextStart {
                    id: "text-0".into(),
                });
            }
            out.push(LlmEvent::TextDelta {
                id: "text-0".into(),
                text: content.into(),
            });
        }

        if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                accumulate_tool_call(&mut self.tool_calls, tc);
            }
        }
    }

    fn close_in_flight(&mut self, out: &mut Vec<LlmEvent>) {
        if self.reasoning_started {
            self.reasoning_started = false;
            out.push(LlmEvent::ReasoningEnd {
                id: "reason-0".into(),
            });
        }
        if self.text_started {
            out.push(LlmEvent::TextEnd {
                id: "text-0".into(),
            });
        }
    }

    /// Produce the terminal StepFinish and Finish events once the stream ends.
    pub(crate) fn finish_events(self) -> Vec<LlmEvent> {
        match self.finish_reason {
            Some(reason) => vec![
                LlmEvent::StepFinish {
                    index: 0,
                    reason: reason.clone(),
                    usage: self.final_usage.clone(),
                },
                LlmEvent::Finish {
                    reason,
                    usage: self.final_usage,
                },
            ],
            None => abnormal_close_events(
                self.reasoning_started,
                self.text_started,
                self.tool_calls,
                self.final_usage,
            ),
        }
    }
}

#[cfg(test)]
#[path = "stream_decoder_tests.rs"]
mod tests;
