//! The live model-output path for the ACP server. Each response delta the
//! runner reports is queued for the serve loop, so a client renders the turn
//! as the model writes it instead of receiving the whole turn after it ends.
//!
//! The queue is unbounded because the producer runs inside the model stream
//! while the consumer is a select branch polled only during a run. A bounded
//! queue would block the producer inside the run once a slow client filled
//! it.

use std::sync::Arc;

use houyicoder_api::agent_event::EventHandler;
use houyicoder_api::agent_event::ResponseStreamEvent;
use houyicoder_core::agent::Runner;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

/// The producer half of the live path. Install it on the runner; each
/// response delta it receives is queued for the serve loop.
#[derive(Clone)]
pub struct AcpStreamSink {
    tx: UnboundedSender<ResponseStreamEvent>,
}

impl AcpStreamSink {
    /// Install this sink as the runner's response-stream destination, keeping
    /// any other handlers the runner already has. Merging rather than
    /// replacing means a composition root that later adds a run-lifecycle or
    /// user-notice handler does not lose it when the live sink is installed.
    pub fn install_on(&self, runner: &mut Runner) {
        let mut handlers = runner.event_handlers();
        handlers.set_response_stream(Arc::new(self.clone()));
        runner.set_event_handlers(handlers);
    }
}

impl EventHandler<ResponseStreamEvent> for AcpStreamSink {
    fn handle(&self, event: ResponseStreamEvent) {
        if self.tx.send(event).is_err() {
            // The serve loop is gone, so the turn has nobody left to deliver
            // to. A debug line keeps the drop observable without treating a
            // closed connection as a failure.
            tracing::debug!("live delta dropped: the serve loop is gone");
        }
    }
}

/// The consumer half: the serve loop reads deltas from here while a run is
/// in flight. None means every sink has dropped.
pub struct AcpLiveStream {
    rx: UnboundedReceiver<ResponseStreamEvent>,
}

impl AcpLiveStream {
    /// Build a connected producer and consumer pair.
    pub fn channel() -> (AcpStreamSink, AcpLiveStream) {
        let (tx, rx) = mpsc::unbounded_channel();
        (AcpStreamSink { tx }, AcpLiveStream { rx })
    }

    /// Build a consumer from an existing receiver, so a caller that has its
    /// own producer (a broadcast forwarding task, for instance) can feed the
    /// serve loop without installing a new handler on the runner.
    pub fn from_receiver(rx: UnboundedReceiver<ResponseStreamEvent>) -> Self {
        AcpLiveStream { rx }
    }

    /// Read the next delta. None when every sink has been dropped.
    pub async fn recv(&mut self) -> Option<ResponseStreamEvent> {
        self.rx.recv().await
    }

    /// Take a delta that is already queued, without waiting. None when the
    /// channel is empty. The serve loop calls this after a run resolves to
    /// drain any deltas the model pushed in its final frame before the run
    /// future was ready, so the last few characters are not lost when the
    /// durable Assistant message is suppressed.
    pub fn try_recv(&mut self) -> Option<ResponseStreamEvent> {
        self.rx.try_recv().ok()
    }
}

#[cfg(test)]
mod tests;
