//! Durable model-step timing events: time to first token and decode time.

use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_protocol::llm::LlmEvent;

use super::{RunError, Runner};
use crate::agent::append::new_event;

/// The time a stream spent inside its reasoning and reply blocks.
///
/// A block is measured from the event that opens it to the event that closes
/// it, so what a figure says is what the provider marked, not a share of the
/// total estimated from text or tokens. A block the call ended inside is left
/// out: an unfinished span is unknown, and zero would be a measurement.
#[derive(Default)]
pub(crate) struct BlockSpans {
    open_reasoning: Option<std::time::Instant>,
    open_reply: Option<std::time::Instant>,
    reasoning_ms: u64,
    response_ms: u64,
    closed_reasoning: bool,
    closed_reply: bool,
}

impl BlockSpans {
    pub(crate) fn note(&mut self, ev: &LlmEvent) {
        match ev {
            LlmEvent::ReasoningStart { .. } => {
                self.open_reasoning = Some(std::time::Instant::now());
            }
            LlmEvent::ReasoningEnd { .. } => {
                if let Some(started) = self.open_reasoning.take() {
                    self.reasoning_ms += started.elapsed().as_millis() as u64;
                    self.closed_reasoning = true;
                }
            }
            LlmEvent::TextStart { .. } => {
                self.open_reply = Some(std::time::Instant::now());
            }
            LlmEvent::TextEnd { .. } => {
                if let Some(started) = self.open_reply.take() {
                    self.response_ms += started.elapsed().as_millis() as u64;
                    self.closed_reply = true;
                }
            }
            _ => {}
        }
    }

    /// The reasoning time, when a block was closed.
    pub(crate) fn reasoning_ms(&self) -> Option<u64> {
        self.closed_reasoning.then_some(self.reasoning_ms)
    }

    /// The reply time, when a block was closed.
    pub(crate) fn response_ms(&self) -> Option<u64> {
        self.closed_reply.then_some(self.response_ms)
    }
}

impl Runner {
    /// Append model step timing for latency analysis.
    ///
    /// The split is what the provider's own stream marked: how long it spent
    /// inside reasoning blocks and inside reply blocks. A block the call ended
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
#[path = "timing_tests.rs"]
mod tests;
