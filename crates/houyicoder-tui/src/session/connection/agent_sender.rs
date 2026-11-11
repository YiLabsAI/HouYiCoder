//! The channel the driver announces on, paired with the wake each arrival
//! needs. A message that lands without a wake waits for a frame timer, and an
//! idle session has none.

use std::sync::Arc;
use std::sync::mpsc::{self, SendError};

use crate::agent_message::SessionMessage;
use crate::app::wake::{UiWake, ui_wake};

/// The driver's outbound channel to the run loop.
#[derive(Clone)]
pub(crate) struct AgentSender {
    events: mpsc::Sender<SessionMessage>,
    wake: Arc<UiWake>,
}

impl AgentSender {
    /// Wrap the session's channel, waking the process-wide run loop.
    pub(crate) fn new(events: mpsc::Sender<SessionMessage>) -> Self {
        Self::with_wake(events, Arc::clone(ui_wake()))
    }

    /// Wrap the channel with a wake the caller owns, for a loop the caller
    /// drives itself.
    pub(crate) fn with_wake(events: mpsc::Sender<SessionMessage>, wake: Arc<UiWake>) -> Self {
        Self { events, wake }
    }

    /// Send one message, waking the loop only when it was accepted. A refused
    /// send means the loop is gone, so there is nothing to wake. The refusal
    /// carries the message back boxed: no caller reads it, and the box keeps
    /// the result small enough to return without copying the payload.
    pub(crate) fn send(
        &self,
        message: SessionMessage,
    ) -> Result<(), Box<SendError<SessionMessage>>> {
        let sent = self.events.send(message).map_err(Box::new);
        if sent.is_ok() {
            self.wake.wake();
        }
        sent
    }
}
