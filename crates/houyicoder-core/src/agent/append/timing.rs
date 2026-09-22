//! Durable model-step timing events: time to first token and decode time.

use houyicoder_context::{SessionEvent, SessionId};

use super::{RunError, Runner};
use crate::agent::append::new_event;

impl Runner {
    /// Append model step timing for TTFT and decode latency analysis.
    pub(crate) async fn append_model_step_timing(
        &self,
        session: SessionId,
        total_ms: u64,
        ttft_ms: Option<u64>,
        decode_ms: Option<u64>,
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
                },
            ))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "timing_tests.rs"]
mod tests;
