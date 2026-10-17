//! Durable model-step timing events: time to first token and decode time.

use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_protocol::llm::LlmEvent;

use super::{RunError, Runner};
use crate::agent::append::new_event;
use std::time::Instant;

/// The time a stream spent inside its reasoning blocks and its output blocks.
///
/// A block is measured from the event that opens it to the event that closes
/// it, so what a figure says is what the provider marked, not a share of the
/// total estimated from text or tokens. An output block is a reply's text or a
/// tool call's argument stream; both are the model producing output, and a call
/// that answers with a tool alone has no text block to measure. Tool calls
/// streaming at the same time are one span from the first opener to the last
/// closer, so overlapping output is counted once. A block the call ended
/// inside is left out: an unfinished span is unknown, and zero would be a
/// measurement.
#[derive(Default)]
pub(crate) struct BlockSpans {
    open_reasoning: Option<Instant>,
    open_text: Option<Instant>,
    open_tool_input: Option<Instant>,
    tool_inputs_open: usize,
    reasoning_ms: u64,
    response_ms: u64,
    closed_reasoning: bool,
    closed_output: bool,
}

/// Whether an event marks the model's output beginning to arrive: a reply or
/// reasoning delta, the opening of a tool call's arguments or a fragment of
/// them, or the call itself when a provider publishes one without fragments.
/// Time to first token is read at the first such event, so a turn the model
/// answers with a tool call alone is timed from the moment its arguments start
/// arriving rather than from the closed stream that carries the call.
pub(crate) fn is_first_output(ev: &LlmEvent) -> bool {
    matches!(
        ev,
        LlmEvent::TextDelta { .. }
            | LlmEvent::ReasoningDelta { .. }
            | LlmEvent::ToolInputStart { .. }
            | LlmEvent::ToolInputDelta { .. }
            | LlmEvent::ToolCall { .. }
    )
}

impl BlockSpans {
    /// Note an event, reading the clock now.
    pub(crate) fn note(&mut self, ev: &LlmEvent) {
        self.note_at(ev, Instant::now());
    }

    /// Note an event at a given instant, so a test can state exact spans.
    pub(crate) fn note_at(&mut self, ev: &LlmEvent, now: Instant) {
        match ev {
            LlmEvent::ReasoningStart { .. } => {
                self.open_reasoning = Some(now);
            }
            LlmEvent::ReasoningEnd { .. } => {
                if let Some(started) = self.open_reasoning.take() {
                    // Measured against the instant the caller passed, so a test
                    // states exact spans and the stream measures real ones.
                    self.reasoning_ms += now.saturating_duration_since(started).as_millis() as u64;
                    self.closed_reasoning = true;
                }
            }
            LlmEvent::TextStart { .. } => {
                self.open_text = Some(now);
            }
            LlmEvent::TextEnd { .. } => {
                if let Some(started) = self.open_text.take() {
                    self.response_ms += now.saturating_duration_since(started).as_millis() as u64;
                    self.closed_output = true;
                }
            }
            LlmEvent::ToolInputStart { .. } => {
                self.open_tool_input = self.open_tool_input.or(Some(now));
                self.tool_inputs_open += 1;
            }
            LlmEvent::ToolInputEnd { .. } => {
                self.tool_inputs_open = self.tool_inputs_open.saturating_sub(1);
                if self.tool_inputs_open == 0
                    && let Some(started) = self.open_tool_input.take()
                {
                    self.response_ms += now.saturating_duration_since(started).as_millis() as u64;
                    self.closed_output = true;
                }
            }
            _ => {}
        }
    }

    /// The reasoning time, when a block was closed.
    pub(crate) fn reasoning_ms(&self) -> Option<u64> {
        self.closed_reasoning.then_some(self.reasoning_ms)
    }

    /// The output time, when an output block was closed.
    pub(crate) fn response_ms(&self) -> Option<u64> {
        self.closed_output.then_some(self.response_ms)
    }
}

impl Runner {
    /// Append model step timing for latency analysis.
    ///
    /// The split is what the provider's own stream marked: how long it spent
    /// inside reasoning blocks and inside output blocks. A block the call ended
    /// inside is not counted, so an absent figure is unknown rather than zero.
    pub(crate) async fn append_model_step_timing(
        &self,
        session: SessionId,
        total_ms: u64,
        ttft_ms: Option<u64>,
        decode_ms: Option<u64>,
        reasoning_ms: Option<u64>,
        response_ms: Option<u64>,
    ) -> Result<(), RunError> {
        let (turn, step) = match self.observability.lock() {
            Ok(ol) => ol.turn_coords(),
            Err(_) => (0, 0),
        };
        self.store
            .append(new_event(
                session,
                SessionEvent::ModelStepTiming {
                    turn,
                    step,
                    total_ms,
                    ttft_ms,
                    decode_ms,
                    reasoning_ms,
                    response_ms,
                },
            ))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
