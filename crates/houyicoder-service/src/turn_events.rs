//! The live events a hosted turn produces for its client: model response
//! deltas, tool progress snapshots, and user-visible notices, in the order the
//! run produced them.
//!
//! Every connection subscribed to the run receives them through a bounded
//! broadcast, so a slow reader loses its oldest events instead of growing a
//! queue it keeps for the rest of the connection; the drop is counted and
//! logged, and the durable session log keeps the authoritative copy.

use std::mem::take;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use houyicoder_api::agent_event::{
    EventHandler, ResponseStreamEvent, ToolExecutionEvent, UserNoticeEvent,
};
use houyicoder_core::agent::Runner;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

/// How far one connection may fall behind the run before its oldest events
/// are dropped. A reply can easily exceed this, so the policy is drop-oldest,
/// not fit-in-memory: a connection that reads slower than the run loses the
/// head of the turn and keeps the newest.
const TURN_EVENT_CAPACITY: usize = 512;

/// One live event from a hosted turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStreamEvent {
    /// A piece of the model's response, as the model writes it.
    Response(ResponseStreamEvent),
    /// A progress snapshot from a tool call that is still running.
    ToolProgress(ToolExecutionEvent),
    /// A notice the user has to see.
    Notice(UserNoticeEvent),
}

/// The producer side: installed on the runner, it hands each live event to
/// every connection subscribed to the run.
#[derive(Clone)]
pub struct TurnEvents {
    tx: broadcast::Sender<TurnStreamEvent>,
    /// Events lost upstream of this stream, by a forwarding source that had
    /// to skip them. They count as this connection's loss too, so a turn
    /// damaged before it reached the broadcast still keeps its durable copy.
    /// One counter per stream: the wiring today gives each connection its own
    /// stream, and a shared stream would need one counter per subscriber.
    upstream_lagged: Arc<AtomicU64>,
}

impl Default for TurnEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnEvents {
    /// Build an event stream with no connection subscribed yet.
    pub fn new() -> Self {
        Self {
            tx: broadcast::channel(TURN_EVENT_CAPACITY).0,
            upstream_lagged: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Route this runner's live events through this stream. A handler the
    /// runner already holds for another domain stays installed, so a
    /// composition root that added one does not lose it here.
    pub fn install_on(&self, runner: &mut Runner) {
        let mut handlers = runner.event_handlers();
        handlers.set_response_stream(Arc::new(self.clone()));
        handlers.set_tool_execution(Arc::new(self.clone()));
        handlers.set_user_notice(Arc::new(self.clone()));
        runner.set_event_handlers(handlers);
    }

    /// Receive every event produced from now on. A receiver that falls
    /// further behind than the stream's capacity loses its oldest events; the
    /// loss is counted on the receiver and logged when it is read past.
    pub fn subscribe(&self) -> TurnEventSubscriber {
        TurnEventSubscriber {
            rx: self.tx.subscribe(),
            dropped: 0,
            upstream_lagged: Arc::clone(&self.upstream_lagged),
        }
    }

    /// Record events a forwarding source had to skip. They reach the same
    /// count a subscriber reads as its own loss, so a turn damaged before it
    /// arrived keeps its durable copy.
    pub(crate) fn note_upstream_lag(&self, count: u64) {
        self.upstream_lagged.fetch_add(count, Ordering::Relaxed);
        tracing::warn!(
            dropped = count,
            "live turn source fell behind; oldest events dropped upstream"
        );
    }

    /// Offer one event to the subscribers, reporting whether any received it.
    /// No subscriber is not a failure: the connection that would have read
    /// it can read the turn from the durable log instead.
    pub(crate) fn publish(&self, event: TurnStreamEvent) -> bool {
        match self.tx.send(event) {
            Ok(_) => true,
            Err(_) => {
                tracing::debug!("turn event dropped: no connection subscribed");
                false
            }
        }
    }
}

impl EventHandler<ResponseStreamEvent> for TurnEvents {
    fn handle(&self, event: ResponseStreamEvent) {
        self.publish(TurnStreamEvent::Response(event));
    }
}

impl EventHandler<ToolExecutionEvent> for TurnEvents {
    fn handle(&self, event: ToolExecutionEvent) {
        self.publish(TurnStreamEvent::ToolProgress(event));
    }
}

impl EventHandler<UserNoticeEvent> for TurnEvents {
    fn handle(&self, event: UserNoticeEvent) {
        self.publish(TurnStreamEvent::Notice(event));
    }
}

/// The consumer side, held by one connection.
pub struct TurnEventSubscriber {
    rx: broadcast::Receiver<TurnStreamEvent>,
    dropped: u64,
    upstream_lagged: Arc<AtomicU64>,
}

impl TurnEventSubscriber {
    /// The next event, in production order. None when the producer is gone.
    ///
    /// Falling behind is not an error: the events this receiver missed are
    /// counted and skipped so the newest ones still arrive, and the loss is
    /// logged so it stays visible.
    pub async fn recv(&mut self) -> Option<TurnStreamEvent> {
        loop {
            match self.rx.recv().await {
                Ok(event) => return Some(event),
                Err(RecvError::Lagged(count)) => self.note_drop(count),
                Err(_) => return None,
            }
        }
    }

    /// Take an event that is already queued, without waiting. None when
    /// nothing is queued or the producer is gone.
    pub fn try_recv(&mut self) -> Option<TurnStreamEvent> {
        loop {
            match self.rx.try_recv() {
                Ok(event) => return Some(event),
                Err(TryRecvError::Lagged(count)) => self.note_drop(count),
                Err(_) => return None,
            }
        }
    }

    /// The number of events this connection fell behind on since the last
    /// read of this value, and reset it. A connection that lost events must
    /// stop suppressing the durable copy of the turn, because the copy is the
    /// only place the missing text still exists.
    pub(crate) fn take_lagged(&mut self) -> u64 {
        let upstream = self.upstream_lagged.swap(0, Ordering::Relaxed);
        take(&mut self.dropped).saturating_add(upstream)
    }

    fn note_drop(&mut self, count: u64) {
        self.dropped = self.dropped.saturating_add(count);
        tracing::warn!(
            dropped = count,
            total = self.dropped,
            "turn stream fell behind; oldest events dropped"
        );
    }
}

#[cfg(test)]
mod tests;
