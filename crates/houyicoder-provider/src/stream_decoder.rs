//! State tracking for an in-flight OpenAI-compatible SSE completion stream.

use houyicoder_protocol::llm::{LlmEvent, Usage};
use serde_json::Value;

use crate::openai_compat::{
    ToolCallAccum, abnormal_close_events, accumulate_tool_call, finalize_tool_calls,
};
use crate::usage::parse_usage;
use std::collections::HashSet;
use std::mem::take;

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
    /// The tool-call slots whose argument stream has been opened, so a slot
    /// opens once and closes once no matter how many fragments arrive.
    tool_inputs_open: HashSet<usize>,
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
            self.close_tool_input(out);
            for ev in finalize_tool_calls(take(&mut self.tool_calls)) {
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
                self.note_tool_fragment(tc, out);
            }
        }
    }

    /// Merge one streamed tool-call fragment and publish the events it implies:
    /// the first fragment that names a call opens that call's input, and any
    /// fragment carrying argument text extends it. A fragment arriving before
    /// the name cannot open the stream, because the event names the call it
    /// belongs to; the arguments it carried are published with the opener, so
    /// the fragments a reader sees always spell out the finalized call.
    fn note_tool_fragment(&mut self, tc: &Value, out: &mut Vec<LlmEvent>) {
        let idx = accumulate_tool_call(&mut self.tool_calls, tc);
        let args = tc
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let slot = &self.tool_calls[idx];
        if !self.tool_inputs_open.contains(&idx) {
            if slot.name.is_empty() {
                return;
            }
            self.tool_inputs_open.insert(idx);
            let (id, name, carried) = (slot.id.clone(), slot.name.clone(), slot.args.clone());
            out.push(LlmEvent::ToolInputStart {
                id: id.clone(),
                name: name.clone(),
            });
            if !carried.is_empty() {
                out.push(LlmEvent::ToolInputDelta {
                    id,
                    name,
                    text: carried,
                });
            }
            return;
        }
        if let Some(args) = args {
            out.push(LlmEvent::ToolInputDelta {
                id: slot.id.clone(),
                name: slot.name.clone(),
                text: args.to_string(),
            });
        }
    }

    /// Close the tool input stream of every call this decoder opened, in slot order.
    /// A reader of the event sequence then sees the arguments end before the
    /// call they built, and a measured block has both an opener and a closer.
    fn close_tool_input(&mut self, out: &mut Vec<LlmEvent>) {
        let mut open: Vec<usize> = self.tool_inputs_open.drain().collect();
        open.sort_unstable();
        for idx in open {
            if let Some(slot) = self.tool_calls.get(idx) {
                out.push(LlmEvent::ToolInputEnd {
                    id: slot.id.clone(),
                    name: slot.name.clone(),
                });
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
    /// A stream that ended without a finish reason still ends every input
    /// stream this decoder opened, so the argument stream of a cut call is
    /// measured rather than left open; a stream that announced its end closed
    /// them where the reason arrived.
    pub(crate) fn finish_events(mut self) -> Vec<LlmEvent> {
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
            None => {
                let mut events = Vec::new();
                self.close_tool_input(&mut events);
                events.extend(abnormal_close_events(
                    self.reasoning_started,
                    self.text_started,
                    self.tool_calls,
                    self.final_usage,
                ));
                events
            }
        }
    }
}

#[cfg(test)]
mod tests;
