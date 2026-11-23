//! The outbound session/update stream: durable turn records and the run's
//! live events (response deltas, paced tool progress, user notices), each
//! wrapped in the session notification envelope and sent in the order the
//! run produced it.

use std::future::pending;
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::AcpServer;
use crate::acp::pump::AcpDuplexPump;
use crate::protocol_adapter::{
    map_notice, map_session_update, map_stream_event, map_tool_progress, tool_title,
};
use crate::turn_events::{TurnEventSubscriber, TurnStreamEvent};
use houyicoder_api::agent_event::{ResponseStreamEvent, ToolExecutionEvent, UserNoticeEvent};
use houyicoder_api::session::TrajectoryPages;
use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::session_update::SessionUpdate;

/// A session/update notification in the envelope the base protocol defines:
/// the session it belongs to, and the update nested under its own key. A
/// client that reads the envelope drops an update sent without it.
pub(super) fn session_notification(
    session: SessionId,
    update: &SessionUpdate,
) -> Result<AcpNotification, ProtocolError> {
    let params = serde_json::json!({
        "sessionId": session.to_string(),
        "update": serde_json::to_value(update).map_err(|e| {
            ProtocolError::new(ErrorCategory::InvalidFrame, e.to_string(), false)
        })?,
    });
    Ok(AcpNotification::new("session/update", params))
}

/// The next live event, or pending forever when no stream is installed. A
/// select branch that never resolves keeps the run future and the frame
/// reader as the only live branches, so the absence of a stream costs
/// nothing. A stream whose producer is gone is dropped here rather than
/// polling a closed receiver on every turn of the loop.
pub(super) async fn next_turn_event(events: &mut Option<TurnEventSubscriber>) -> TurnStreamEvent {
    if let Some(stream) = events.as_mut()
        && let Some(event) = stream.recv().await
    {
        return event;
    }
    *events = None;
    pending().await
}

impl AcpServer {
    /// Read every durable event appended since the last drain, advancing the
    /// pushed cursor past them. An absolute ordinal of history means drops cannot
    /// re-send records the client already saw as soon as the window moved. A
    /// read that fails leaves the position after the records it did report, so
    /// the next pass resumes there instead of repeating them.
    pub(super) fn drain_pushed(&mut self) -> Vec<SessionLogEntry> {
        let store = self.runner.store();
        let pages = TrajectoryPages::new(store.as_ref(), self.session, self.pushed_through);
        let mut events = Vec::new();
        for page in pages {
            match page {
                Ok(page) => {
                    self.pushed_through = page.next;
                    events.extend(page.events);
                }
                Err(e) => {
                    tracing::warn!("durable read for session {} stopped: {e}", self.session);
                    break;
                }
            }
        }
        events
    }

    /// Push every durable turn event the client has not seen yet, in the order
    /// the engine appended them.
    pub(super) async fn push_pending(
        &mut self,
        io: &mut AcpDuplexPump,
    ) -> Result<(), ProtocolError> {
        let events = self.drain_pushed();
        for ev in &events {
            self.push_durable_event(io, ev).await?;
        }
        Ok(())
    }

    /// Echo the prompt the client just sent as a user message chunk, so the
    /// host renders it before the answer rather than after it.
    pub(super) async fn echo_user_prompt(
        &self,
        io: &mut AcpDuplexPump,
        text: &str,
    ) -> Result<(), ProtocolError> {
        let event = SessionEvent::UserInput {
            text: text.to_string(),
        };
        if let Some(update) = map_session_update(&event) {
            let notification = session_notification(self.session, &update)?;
            self.send_typed(io, &notification).await?;
        }
        Ok(())
    }

    /// Forward one engine turn event as a session/update notification.
    pub(super) async fn push_durable_event(
        &mut self,
        io: &mut AcpDuplexPump,
        ev: &SessionLogEntry,
    ) -> Result<(), ProtocolError> {
        if matches!(ev.event, SessionEvent::UserInput { .. }) {
            return Ok(());
        }
        if let SessionEvent::ToolResult { call_id, .. } = &ev.event {
            self.completed_calls.insert(call_id.clone());
        }
        if self.suppresses_durable(&ev.event) {
            return Ok(());
        }
        if let Some(mut update) = map_session_update(&ev.event) {
            if let (SessionEvent::ToolCall { tool, input, .. }, SessionUpdate::ToolCall(call)) =
                (&ev.event, &mut update)
            {
                call.title = tool_title(tool, Some(input));
            }
            let notification = session_notification(self.session, &update)?;
            self.send_typed(io, &notification).await?;
        }
        Ok(())
    }

    /// Whether a durable turn record repeats text the live stream already sent.
    pub(super) fn suppresses_durable(&self, event: &SessionEvent) -> bool {
        !self.stream_lossy
            && self.streamed_text.load(Ordering::Relaxed)
            && matches!(
                event,
                SessionEvent::AssistantMessage { .. } | SessionEvent::Reasoning { .. }
            )
    }

    /// Forward one live response delta as a session/update notification.
    pub(super) async fn push_delta(
        &self,
        io: &mut AcpDuplexPump,
        delta: &ResponseStreamEvent,
    ) -> Result<(), ProtocolError> {
        if matches!(
            delta,
            ResponseStreamEvent::AssistantTextDelta { .. }
                | ResponseStreamEvent::ReasoningDelta { .. }
        ) {
            self.streamed_text.store(true, Ordering::Relaxed);
        }
        if let Some(update) = map_stream_event(delta) {
            let notification = session_notification(self.session, &update)?;
            self.send_typed(io, &notification).await?;
        }
        Ok(())
    }
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
        let notification = session_notification(self.session, &map_tool_progress(progress))?;
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
        let notification = session_notification(self.session, &map_notice(notice))?;
        self.send_typed(io, &notification).await
    }
}
