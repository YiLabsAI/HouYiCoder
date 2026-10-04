//! Protocol bridge between the synchronous TUI and its asynchronous server.
//! SessionConnection owns the channels and driver task; App owns transcript
//! state.

use std::cell::Cell;
use std::fmt;
use std::sync::mpsc;
use std::time::Duration;

use houyicoder_protocol::envelope::RequestId;

use crate::agent_message::{ClientCommand, SessionMessage};

mod driver;

#[cfg(test)]
pub(crate) use driver::{
    cancel_child_turn_notification, drive_client, inject_child_notification, inject_notification,
    kill_all_notification, kill_child_notification, queue_remove_notification,
};

/// The connection's request-identifier sequence is exhausted. Returned once
/// the counter reaches u64::MAX and refused from then on: the counter does
/// not advance past the maximum, so allocation fails rather than wrap onto
/// an identifier already in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestIdExhausted;

impl fmt::Display for RequestIdExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("request ID sequence exhausted")
    }
}

impl std::error::Error for RequestIdExhausted {}

/// A command did not enter the connection queue. Both variants mean the
/// command never left this process: no reply will come, so the caller must
/// not register state waiting on one. NotConnected is reported when no
/// active session exists; Closed when the driver task has exited and its
/// command receiver is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueError {
    NotConnected,
    Closed,
}

impl fmt::Display for EnqueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnqueueError::NotConnected => f.write_str("no active session"),
            EnqueueError::Closed => f.write_str("connection is closed"),
        }
    }
}

impl std::error::Error for EnqueueError {}

/// The lifecycle of one connection. Connecting spans task start to a
/// successful Hello; Ready is confirmed by the handshake, not inferred from
/// the connection object existing. Lost keeps the cause: transcript, status,
/// and other last-observed values stay visible after the loss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// No connection exists (the App holds no SessionConnection).
    Disconnected,
    /// The driver task started; the Hello handshake has not succeeded yet.
    Connecting,
    /// The Hello handshake succeeded; requests and frames flow.
    Ready,
    /// The connection ended. The cause explains what broke.
    Lost(String),
}

/// The result of one non-blocking poll of the inbound message channel.
/// Empty and closed are distinct: empty means no message is ready, closed
/// means the driver is gone and no message will ever arrive again. The
/// message payload is unboxed despite the size: each poll result is
/// consumed immediately by the event loop, never stored or copied.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "consumed immediately per poll, never stored"
)]
pub enum PollOutcome {
    /// A message is ready to apply.
    Message(SessionMessage),
    /// No message is ready right now; the driver is still live.
    Idle,
    /// The message channel closed: the driver task has ended.
    Closed,
}

/// Channels, request identifiers, and driver lifetime for one live connection.
pub struct SessionConnection {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<ClientCommand>,
    agent_rx: mpsc::Receiver<SessionMessage>,
    next_req_id: Cell<u64>,
    exhaustion_reported: Cell<bool>,
    status: ConnectionStatus,
    driver: tokio::task::JoinHandle<()>,
}

impl Drop for SessionConnection {
    fn drop(&mut self) {
        // Dropping cmd_tx signals the driver to exit cleanly (its cmd_rx
        // recv returns None). But the driver may be stuck in a transport
        // await that never returns, so abort the task as well. Abort stops
        // the task at its next yield point; the runtime reclaims it.
        self.driver.abort();
    }
}

impl SessionConnection {
    /// Spawn the protocol driver and retain the application-facing channels.
    pub fn spawn(
        client: houyicoder_client::Client,
        agent_tx: mpsc::Sender<SessionMessage>,
        agent_rx: mpsc::Receiver<SessionMessage>,
        runtime: &tokio::runtime::Runtime,
    ) -> Self {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ClientCommand>();
        let driver = runtime.spawn(driver::drive_client(client, cmd_rx, agent_tx));
        Self {
            cmd_tx,
            agent_rx,
            next_req_id: Cell::new(0),
            exhaustion_reported: Cell::new(false),
            status: ConnectionStatus::Connecting,
            driver,
        }
    }

    /// Test-only connection whose inbound side is the given receiver. No
    /// protocol driver runs: the test's own sender is the only message
    /// source, so a poll sees exactly what the test queued, in order. The
    /// spawned task only consumes commands, keeping the outbound side open.
    #[cfg(test)]
    pub(crate) fn from_receiver(agent_rx: mpsc::Receiver<SessionMessage>) -> Self {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ClientCommand>();
        let driver = crate::composition::shared_runtime()
            .spawn(async move { while cmd_rx.recv().await.is_some() {} });
        Self {
            cmd_tx,
            agent_rx,
            next_req_id: Cell::new(0),
            exhaustion_reported: Cell::new(false),
            status: ConnectionStatus::Ready,
            driver,
        }
    }

    /// Issue a connection-local monotonic request identifier, starting at
    /// zero and advancing by one per call. When the sequence exhausts the
    /// u64 range the counter stops advancing and allocation is refused, so a
    /// fresh connection always starts its own sequence and an exhausted one
    /// never reuses an identifier in flight.
    pub fn next_request_id(&self) -> Result<RequestId, RequestIdExhausted> {
        let id = self.next_req_id.get();
        let Some(next) = id.checked_add(1) else {
            return Err(RequestIdExhausted);
        };
        self.next_req_id.set(next);
        Ok(RequestId(id))
    }

    /// Consume the one-shot exhaustion notice. True only on the first call so
    /// an auto path that observes the exhausted sequence announces it once
    /// and later rounds stay quiet instead of repeating the failure. The flag
    /// is sticky, so the method itself only flips it; the caller decides
    /// whether to report.
    pub(crate) fn take_exhaustion_notice(&self) -> bool {
        if self.exhaustion_reported.get() {
            return false;
        }
        self.exhaustion_reported.set(true);
        true
    }

    /// Test-only seam to drive the counter to a boundary (the u64 ceiling is
    /// unreachable by repeated allocation in a real run).
    #[cfg(test)]
    pub(crate) fn set_next_req_id(&self, v: u64) {
        self.next_req_id.set(v);
    }

    /// Enqueue a command for wire translation. Ok means the command entered
    /// the local connection queue, not that it reached the transport or
    /// server. Err when the driver task is gone (its receiver dropped): the
    /// command never left this process, so the caller knows no reply will
    /// come and must not register state waiting on one.
    pub fn enqueue(&self, cmd: ClientCommand) -> Result<(), EnqueueError> {
        self.cmd_tx.send(cmd).map_err(|_| EnqueueError::Closed)
    }

    /// The current connection lifecycle state.
    pub fn status(&self) -> &ConnectionStatus {
        &self.status
    }

    /// Confirm the Hello handshake: Connecting becomes Ready. Returns false
    /// (and changes nothing) on every other state, so a late confirmation
    /// cannot revive a lost connection and a repeat is idempotent.
    pub(crate) fn mark_ready(&mut self) -> bool {
        if matches!(self.status, ConnectionStatus::Connecting) {
            self.status = ConnectionStatus::Ready;
            return true;
        }
        false
    }

    /// Record the loss, keeping the first cause. Returns true only on the
    /// transition into Lost, so a second observation neither overwrites the
    /// original cause nor repeats the announcement.
    pub(crate) fn mark_lost(&mut self, cause: String) -> bool {
        if matches!(self.status, ConnectionStatus::Lost(_)) {
            return false;
        }
        self.status = ConnectionStatus::Lost(cause);
        true
    }

    /// Take one pending inbound message without blocking. Idle (no message
    /// ready) and Closed (driver gone, no message will ever arrive) are
    /// distinct outcomes: folding them hid the difference between a quiet
    /// moment and a dead connection.
    pub fn poll(&mut self) -> PollOutcome {
        match self.agent_rx.try_recv() {
            Ok(msg) => PollOutcome::Message(msg),
            Err(mpsc::TryRecvError::Empty) => PollOutcome::Idle,
            Err(mpsc::TryRecvError::Disconnected) => PollOutcome::Closed,
        }
    }

    /// Block up to the timeout for the next inbound message during startup.
    pub fn poll_startup(&mut self, timeout: Duration) -> Option<SessionMessage> {
        self.agent_rx.recv_timeout(timeout).ok()
    }
}

#[cfg(test)]
#[path = "connection_tests.rs"]
mod connection_tests;
