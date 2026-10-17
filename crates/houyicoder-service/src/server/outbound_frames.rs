//! The outbound frame path of one server connection: durable events mapped
//! to sequenced frames, responses, and protocol errors sent to the client.

use crate::protocol_adapter::{map_acpx_notification, map_session_update};
use houyicoder_api::session::SessionLog;
use houyicoder_api::session::TrajectoryPages;
use houyicoder_api::session::TrajectoryPosition;
use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use houyicoder_protocol::envelope::{
    DurableEventId, EventEnvelope, RequestId, ResponseEnvelope, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::{FrontendEvent, PendingInputId, QueuedInput};

use super::event_sequencer::EntryPayloads;
use super::{FrameCarrier, Server};
use houyicoder_protocol::envelope::ChildTranscriptFrame;

/// What the user reads where an event was too large to send. The row
/// states where the content still is, so the notice is not a dead end.
const OVERSIZED_EVENT_NOTICE: &str =
    "an event exceeded the protocol's per-message limit — the session log holds the full entry";

impl Server {
    fn map_turn_event(event: &SessionLogEntry) -> EntryPayloads {
        let mut payloads = Vec::with_capacity(3);
        if let Some(update) = map_session_update(&event.event) {
            payloads.push(FrontendEvent::SessionUpdate { update });
        }
        if let Some(notification) = map_acpx_notification(&event.event) {
            payloads.push(FrontendEvent::Acpx { notification });
        }
        if let SessionEvent::MidTurnInput {
            text,
            pending_input_id: Some(id),
        } = &event.event
        {
            payloads.push(FrontendEvent::QueuedInputCommitted {
                inputs: vec![QueuedInput {
                    id: PendingInputId(*id),
                    text: text.clone(),
                }],
            });
        }
        EntryPayloads {
            event_id: DurableEventId(event.id.to_string()),
            payloads,
        }
    }

    /// Sequence and send every event currently available for this session.
    /// The sequencer holds its producer lock while reading the durable cursor
    /// and draining runtime events, so a delta emitted after an append remains
    /// behind that append's mappings.
    pub(super) async fn flush_events(
        &mut self,
        io: &mut FrameCarrier,
    ) -> Result<(), ProtocolError> {
        let runner = self.runner.clone();
        let session = self.session;
        let sequencer = self.event_sequencer.clone();
        let frames = sequencer.sequence_available(move |cursor| {
            Self::load_entries(runner.store().as_ref(), session, cursor)
        });
        for frame in frames {
            Self::send_event_frame(io, frame).await?;
        }
        Ok(())
    }

    /// Replay reliable history requested by Hello, then send events that
    /// accumulated while no connection was active.
    pub(super) async fn replay_events(
        &mut self,
        io: &mut FrameCarrier,
    ) -> Result<(), ProtocolError> {
        let runner = self.runner.clone();
        let session = self.session;
        let sequencer = self.event_sequencer.clone();
        let replay = sequencer.prepare_replay(self.replay_after, move |cursor| {
            Self::load_entries(runner.store().as_ref(), session, cursor)
        });
        for frame in replay {
            Self::send_event_frame(io, frame).await?;
        }
        self.flush_events(io).await
    }

    /// Map the durable records from an ordinal to the end of the history.
    ///
    /// The log is never resident whole: the records are read page by page and
    /// mapped as each page arrives. The mappings this call covers are held
    /// until the caller sequences them, so a replay asked from the start of a
    /// long session holds the frames it is about to send. A read that fails
    /// leaves the cursor after the last record it reported, so the next round
    /// resumes there rather than replaying the records the client already saw.
    fn load_entries(
        store: &dyn SessionLog,
        session: SessionId,
        cursor: usize,
    ) -> Vec<EntryPayloads> {
        let pages = TrajectoryPages::new(
            store,
            session,
            TrajectoryPosition {
                index: cursor,
                byte_offset: None,
            },
        );
        let mut loaded = Vec::new();
        for page in pages {
            match page {
                Ok(page) => loaded.extend(page.events.iter().map(Self::map_turn_event)),
                Err(e) => {
                    tracing::warn!("durable read for session {session} stopped: {e}");
                    break;
                }
            }
        }
        loaded
    }

    /// Deliver one sequenced event, replacing it with a bounded notice when
    /// its payload exceeds the protocol's per-message bound. The replacement keeps
    /// the event's stream position, so a consumer reading the seq stream finds
    /// a row where the event was rather than a gap.
    async fn send_event_frame(
        io: &mut FrameCarrier,
        frame: EventEnvelope,
    ) -> Result<(), ProtocolError> {
        let seq = frame.seq;
        match Self::send_typed(io, &ServerFrame::Event(frame)).await {
            Err(e) if e.category == ErrorCategory::MessageTooLarge => {
                // Propagating the refusal would end the connection, and the
                // event stays in the durable log, so a reconnect would replay
                // it into the same refusal forever. The notice ends that loop
                // and says where the content still is.
                tracing::warn!(
                    "event seq {} exceeded the per-message byte bound: {e}",
                    seq.0
                );
                let notice = EventEnvelope::new(
                    seq,
                    FrontendEvent::SystemLine {
                        text: OVERSIZED_EVENT_NOTICE.to_string(),
                    },
                );
                Self::send_typed(io, &ServerFrame::Event(notice)).await
            }
            other => other,
        }
    }

    /// Map a fetched child session's turn events through the same adapters
    /// used by the parent transcript.
    pub(super) async fn child_transcript_frames(
        &self,
        child_sid: &houyicoder_protocol::frontend::SessionId,
    ) -> Vec<ChildTranscriptFrame> {
        let Some(sid) = houyicoder_context::SessionId::from_display_string(&child_sid.0) else {
            return Vec::new();
        };
        let events = self.runner.store().replay(sid).await.unwrap_or_default();
        let mut frames = Vec::with_capacity(events.len());
        for event in &events {
            if let Some(update) = map_session_update(&event.event) {
                frames.push(ChildTranscriptFrame::Session(update));
            }
            if let Some(notification) = map_acpx_notification(&event.event) {
                frames.push(ChildTranscriptFrame::Acpx(notification));
            }
        }
        frames
    }

    /// Send a response paired to a request by id.
    pub(super) async fn send_response(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        payload: ResponsePayload,
    ) -> Result<(), ProtocolError> {
        let frame = ServerFrame::Response(ResponseEnvelope::new(req_id, payload));
        Self::send_typed(io, &frame).await
    }

    /// Send a protocol error as an unpaired response.
    pub(super) async fn send_protocol_error(
        &mut self,
        io: &mut FrameCarrier,
        err: ProtocolError,
    ) -> Result<(), ProtocolError> {
        self.send_response(io, RequestId(u64::MAX), ResponsePayload::Error(err))
            .await
    }

    /// Encode a typed server frame and push it through the carrier. The
    /// encoder enforces the protocol's per-message bound, so an over-bound frame
    /// is refused before any carrier sees it and the caller reports a local
    /// refusal rather than an unknown delivery outcome.
    pub(super) async fn send_typed<T: serde::Serialize>(
        io: &mut FrameCarrier,
        msg: &T,
    ) -> Result<(), ProtocolError> {
        let frame = encode(msg)?;
        io.send_frame(frame).await
    }
}

#[cfg(test)]
mod tests;
