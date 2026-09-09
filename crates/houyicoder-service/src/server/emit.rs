//! Sequenced projection and frame emission for one server connection.

use crate::protocol_adapter::{map_acpx_notification, map_session_update};
use houyicoder_context::{SessionEvent, SessionLogEntry};
use houyicoder_protocol::envelope::{
    EventEnvelope, RequestId, ResponseEnvelope, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::framing::{FrameError, encode};
use houyicoder_protocol::frontend::{FrontendEvent, PendingInputId, QueuedInput};
use houyicoder_protocol::wire::WireError;

use super::{Server, ServerIo};

impl Server {
    fn project_turn_event(event: &SessionLogEntry) -> Vec<FrontendEvent> {
        let mut projected = Vec::with_capacity(3);
        if let Some(update) = map_session_update(&event.event) {
            projected.push(FrontendEvent::SessionUpdate { update });
        }
        if let Some(notification) = map_acpx_notification(&event.event) {
            projected.push(FrontendEvent::Acpx { notification });
        }
        if let SessionEvent::MidTurnInput {
            text,
            pending_input_id: Some(id),
        } = &event.event
        {
            projected.push(FrontendEvent::QueuedInputCommitted {
                inputs: vec![QueuedInput {
                    id: PendingInputId(*id),
                    text: text.clone(),
                }],
            });
        }
        projected
    }

    /// Sequence and send every event currently available for this session.
    /// The sequencer holds its producer lock while reading the durable cursor
    /// and draining runtime events, so a delta emitted after an append remains
    /// behind that append's projections.
    pub(super) async fn flush_events(&mut self, io: &mut ServerIo) -> Result<(), WireError> {
        let runner = self.runner.clone();
        let session = self.session;
        let sequencer = self.event_sequencer.clone();
        let frames = sequencer.sequence_available(move |cursor| {
            runner
                .store()
                .trajectory_since(session, cursor)
                .iter()
                .map(Self::project_turn_event)
                .collect()
        });
        for frame in frames {
            self.send_event_envelope(io, &frame).await?;
        }
        Ok(())
    }

    /// Replay reliable history requested by Hello, then send events that
    /// accumulated while no connection was active.
    pub(super) async fn replay_events(&mut self, io: &mut ServerIo) -> Result<(), WireError> {
        let runner = self.runner.clone();
        let session = self.session;
        let sequencer = self.event_sequencer.clone();
        let replay = sequencer.prepare_replay(self.replay_after, move |cursor| {
            runner
                .store()
                .trajectory_since(session, cursor)
                .iter()
                .map(Self::project_turn_event)
                .collect()
        });
        for frame in replay {
            self.send_event_envelope(io, &frame).await?;
        }
        self.flush_events(io).await
    }

    async fn send_event_envelope(
        &mut self,
        io: &mut ServerIo,
        frame: &EventEnvelope,
    ) -> Result<(), WireError> {
        self.send_typed(io, &ServerFrame::Event(frame.clone()))
            .await
    }

    /// Project a fetched child session's turn events through the same adapters
    /// used by the parent transcript.
    pub(super) async fn child_transcript_frames(
        &self,
        child_sid: &houyicoder_protocol::frontend::SessionId,
    ) -> Vec<houyicoder_protocol::envelope::ChildTranscriptFrame> {
        let Some(sid) = houyicoder_context::SessionId::from_display_string(&child_sid.0) else {
            return Vec::new();
        };
        let events = self.runner.store().replay(sid).await.unwrap_or_default();
        let mut frames = Vec::with_capacity(events.len());
        for event in &events {
            if let Some(update) = map_session_update(&event.event) {
                frames.push(houyicoder_protocol::envelope::ChildTranscriptFrame::Session(update));
            }
            if let Some(notification) = map_acpx_notification(&event.event) {
                frames.push(houyicoder_protocol::envelope::ChildTranscriptFrame::Acpx(
                    notification,
                ));
            }
        }
        frames
    }

    /// Send a response paired to a request by id.
    pub(super) async fn send_response(
        &mut self,
        io: &mut ServerIo,
        req_id: RequestId,
        payload: ResponsePayload,
    ) -> Result<(), WireError> {
        let frame = ServerFrame::Response(ResponseEnvelope::new(req_id, payload));
        self.send_typed(io, &frame).await
    }

    /// Send a wire error as an unpaired response.
    pub(super) async fn send_wire_error(
        &mut self,
        io: &mut ServerIo,
        err: WireError,
    ) -> Result<(), WireError> {
        self.send_response(io, RequestId(u64::MAX), ResponsePayload::Error(err))
            .await
    }

    /// Encode a typed server frame and push it through the carrier.
    pub(super) async fn send_typed<T: serde::Serialize>(
        &mut self,
        io: &mut ServerIo,
        msg: &T,
    ) -> Result<(), WireError> {
        let frame = encode(msg).map_err(frame_to_wire)?;
        io.send_frame(frame).await
    }
}

fn frame_to_wire(error: FrameError) -> WireError {
    WireError::new(
        houyicoder_protocol::wire::WireErrorKind::InvalidFrame,
        error.to_string(),
        false,
    )
}
