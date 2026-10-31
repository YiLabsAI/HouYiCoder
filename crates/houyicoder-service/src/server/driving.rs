//! Keeping a running host operation answered.
//!
//! A model turn, a resume, and a direct shell command share one wait loop:
//! while the work runs, client frames are dispatched, queued events flushed,
//! and child permission asks routed. A client disconnect ends the wait, and
//! what happens to the work then depends on whether it already left a durable
//! trace the log has to answer.

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use futures::future::Either;
use futures::future::pending;
use houyicoder_core::agent::multi_agent::bus_types::BusMessage;
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::envelope::ClientFrame;
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::timeout;

use super::FrameCarrier;
use super::Server;
use super::child_permission;

/// What the wait loop does with the work in flight when the client goes away.
/// The caller picks it when it starts the work, because only the caller knows
/// whether that work already left a durable trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OnClose {
    /// Let the work drop. A model turn appends its own outcome as it goes and
    /// a client that left will not read the answer.
    Abandon,
    /// Cancel the work the way an Escape key press does, and wait for the
    /// result. A direct shell command has already appended its call, so
    /// dropping the future would leave a call no result answers.
    Settle,
}

/// How long a closed connection waits for cancelled work to record its
/// outcome. The cancel resolves a command that honors it promptly; the bound
/// keeps a tool that ignores it from holding the connection teardown open.
const CLOSE_SETTLE_BUDGET: Duration = Duration::from_secs(5);

impl Server {
    /// Drive one host operation to completion while the connection stays live:
    /// client frames are dispatched, queued events flushed, and a client
    /// disconnect ends the wait. A model turn, a resume, and a direct shell
    /// command all run through here, so Esc, event delivery, and disconnect
    /// behave the same whichever one is running.
    pub(super) async fn drive_while<F>(
        &mut self,
        io: &mut FrameCarrier,
        work: F,
        closed: &str,
        permissions: Option<&mut Receiver<BusMessage>>,
        on_close: OnClose,
    ) -> Result<F::Output, ProtocolError>
    where
        F: Future,
        F::Output: Debug,
    {
        tokio::pin!(work);
        let mut permissions = permissions;
        loop {
            let notify_fut = match &self.append_notify {
                Some(n) => Either::Left(n.notified()),
                None => Either::Right(pending::<()>()),
            };
            let event_fut = self.event_sequencer.notified();
            let permission_fut = match permissions.as_deref_mut() {
                Some(receiver) => Either::Left(receiver.recv()),
                None => Either::Right(pending::<Result<BusMessage, RecvError>>()),
            };
            tokio::select! {
                done = &mut work => return Ok(done),
                frame = io.next_frame() => match frame {
                    Ok(Some(f)) => self.dispatch_during_run_frame(io, &f).await,
                    Ok(None) => {
                        if on_close == OnClose::Settle {
                            self.settle_on_close(&mut work).await;
                        }
                        return Err(ProtocolError::new(
                            ErrorCategory::Unavailable,
                            closed,
                            false,
                        ));
                    }
                    // A transport error ends the connection the same way a
                    // clean close does, so the work keeps the same handling.
                    Err(e) => {
                        if on_close == OnClose::Settle {
                            self.settle_on_close(&mut work).await;
                        }
                        return Err(e);
                    }
                },
                _ = notify_fut => self.flush_events(io).await?,
                _ = event_fut => self.flush_events(io).await?,
                req = permission_fut => {
                    if let Ok(msg) = req
                        && let Err(e) = child_permission::handle(self, io, msg).await
                    {
                        tracing::warn!("child permission ask failed: {e}");
                    }
                },
            }
        }
    }

    async fn dispatch_during_run_frame(&mut self, io: &mut FrameCarrier, frame: &str) {
        if let Ok(notif) = serde_json::from_str::<AcpNotification>(frame) {
            self.handle_session_notification(&notif);
        } else if let Ok(ClientFrame::Request(req)) = serde_json::from_str::<ClientFrame>(frame) {
            self.handle_request_during_run(io, req).await;
        }
    }

    /// Cancel the work a disconnected client left running and wait for its
    /// outcome. The cancel is the one an Escape key press issues, so a direct
    /// shell command records the same not-run result it would have recorded
    /// while the client was live. The wait is bounded: a tool that ignores
    /// the cancel must not hold the connection teardown open.
    async fn settle_on_close<F>(&self, work: &mut Pin<&mut F>)
    where
        F: Future,
        F::Output: Debug,
    {
        // The first poll is where the operation installs the cancel handle it
        // watches. Aborting before that poll reaches the slot the previous
        // operation left, or nothing, and the command runs on uncancelled: a
        // client that leaves before its request is first polled would get a
        // command it never saw answered. Work that finishes on this poll has
        // nothing left to cancel.
        if let Poll::Ready(outcome) = futures::poll!(work.as_mut()) {
            tracing::debug!(?outcome, "work finished as the client left");
            return;
        }
        self.runner.abort();
        match timeout(CLOSE_SETTLE_BUDGET, work).await {
            Ok(outcome) => {
                tracing::debug!(?outcome, "cancelled work stopped after the client left")
            }
            Err(_) => tracing::warn!(
                budget_secs = CLOSE_SETTLE_BUDGET.as_secs(),
                "cancelled work did not stop within the settle budget"
            ),
        }
    }
}
