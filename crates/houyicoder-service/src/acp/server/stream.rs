//! Turning the run's live events into session/update frames: response deltas,
//! paced tool progress, and user notices, each sent in the order the run
//! produced it while the turn is live.

use std::time::Instant;

use super::AcpServer;
use super::util::session_update;
use crate::acp::pump::AcpDuplexPump;
use crate::protocol_adapter::{map_notice, map_tool_progress};
use crate::turn_events::{TurnEventSubscriber, TurnStreamEvent};
use houyicoder_api::agent_event::{ToolExecutionEvent, UserNoticeEvent};
use houyicoder_protocol::error::ProtocolError;

impl AcpServer {
    /// Forward one live turn event to the client. A tool update is preceded by
    /// the durable records it belongs to, so the call it updates has already
    /// been sent by the time the update arrives.
    pub(super) async fn push_stream_event(
        &mut self,
        io: &mut AcpDuplexPump,
        event: &TurnStreamEvent,
    ) -> Result<(), ProtocolError> {
        match event {
            TurnStreamEvent::Response(delta) => {
                self.push_delta(io, delta).await?;
                self.push_pending(io).await?;
            }
            TurnStreamEvent::ToolProgress(progress) => {
                self.push_pending(io).await?;
                self.push_progress(io, progress).await?;
            }
            TurnStreamEvent::Notice(notice) => {
                self.push_pending(io).await?;
                self.push_notice(io, notice).await?;
            }
        }
        Ok(())
    }

    /// Record that this turn's live stream dropped events. The durable copy
    /// of the turn then stops being suppressed, so the text a lagging client
    /// missed still reaches it.
    pub(super) fn note_stream_lag(&mut self, events: &mut Option<TurnEventSubscriber>) {
        let lagged = events
            .as_mut()
            .map(TurnEventSubscriber::take_lagged)
            .unwrap_or(0);
        if lagged > 0 && !self.stream_lossy {
            self.stream_lossy = true;
            tracing::warn!(
                dropped = lagged,
                "live turn stream fell behind; the durable copy will be sent"
            );
        }
    }

    /// Send the events the model pushed in the run's final frame after the run
    /// future resolved. Without this the last deltas are lost when the durable
    /// Assistant message is suppressed by streamed_text.
    pub(super) async fn drain_event_tail(
        &mut self,
        io: &mut AcpDuplexPump,
        events: &mut Option<TurnEventSubscriber>,
    ) -> Result<(), ProtocolError> {
        while let Some(event) = events.as_mut().and_then(TurnEventSubscriber::try_recv) {
            self.note_stream_lag(events);
            self.push_stream_event(io, &event).await?;
        }
        Ok(())
    }

    /// Forward one tool progress snapshot, at most once per interval per call.
    /// The throttle keeps a fast-reporting tool from filling the client
    /// stream; the call's completion update carries the final state, and a
    /// snapshot that arrives after it is dropped so a finished call never
    /// reads as still running.
    pub(super) async fn push_progress(
        &mut self,
        io: &mut AcpDuplexPump,
        progress: &ToolExecutionEvent,
    ) -> Result<(), ProtocolError> {
        let ToolExecutionEvent::Progress { call_id, .. } = progress;
        if self.completed_calls.contains(call_id) {
            return Ok(());
        }
        if !self.progress.allow(call_id, Instant::now()) {
            return Ok(());
        }
        let notification = session_update(self.session, &map_tool_progress(progress))?;
        self.send_typed(io, &notification).await
    }

    /// Forward one user-facing notice as agent text. The diagnostic log keeps
    /// the same text for a reader who was not connected.
    async fn push_notice(
        &self,
        io: &mut AcpDuplexPump,
        notice: &UserNoticeEvent,
    ) -> Result<(), ProtocolError> {
        tracing::warn!("user notice: {}", notice.message);
        let notification = session_update(self.session, &map_notice(notice))?;
        self.send_typed(io, &notification).await
    }
}
