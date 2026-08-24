//! Server frame emission: push durable turn events + send typed
//! responses/events on the monotonic seq stream. Extracted from server.rs on
//! size grounds; lives as a child module so an impl block here reaches the
//! Server fields (descendant modules see ancestor private fields).

use std::sync::atomic::Ordering;

use crate::projection::{project_acpx_context, project_session_update};
use houyicoder_context::TurnEvent;
use houyicoder_protocol::envelope::{
    EventEnvelope, EventSeq, RequestId, ResponseEnvelope, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::framing::{FrameError, encode};
use houyicoder_protocol::frontend::FrontendEventKind;
use houyicoder_protocol::wire::WireError;

use super::{Server, ServerIo};

impl Server {
    /// Forward one engine turn event as a typed wire frame. A kind the base
    /// protocol has a standard session/update variant for projects to a
    /// SessionUpdate; a kind with no base counterpart (compaction boundary,
    /// summary, meta user, permission decision) projects to an acpx/context/*
    /// extension notification. The two are orthogonal streams the client
    /// routes by the FrontendEventKind tag, so neither carries opaque engine
    /// JSON — the frontend never imports engine types.
    pub(super) async fn push_turn_event(
        &mut self,
        io: &mut ServerIo,
        ev: &TurnEvent,
    ) -> Result<(), WireError> {
        if let Some(update) = project_session_update(&ev.kind) {
            self.send_event(io, FrontendEventKind::SessionUpdate { update })
                .await?;
        }
        if let Some(notification) = project_acpx_context(&ev.kind) {
            self.send_event(io, FrontendEventKind::Acpx { notification })
                .await?;
        }
        Ok(())
    }

    /// Project a fetched child session's turn events to the same session/update
    /// and acpx frame stream the parent accumulates. The TUI projects the
    /// result through the same pipeline as its own. Mirrors push_turn_event's
    /// dual projection. A sync child is terminal at expand time, so this is a
    /// one-shot snapshot; a missing or unreadable child log returns an empty
    /// list the TUI surfaces as an unavailable line.
    pub(super) async fn child_transcript_frames(
        &self,
        child_sid: &houyicoder_protocol::frontend::SessionId,
    ) -> Vec<houyicoder_protocol::envelope::ChildTranscriptFrame> {
        let Some(sid) = houyicoder_context::SessionId::from_display_string(&child_sid.0) else {
            return Vec::new();
        };
        let events = self.runner.store().replay(sid).await.unwrap_or_default();
        let mut frames = Vec::with_capacity(events.len());
        for ev in &events {
            if let Some(update) = project_session_update(&ev.kind) {
                frames.push(houyicoder_protocol::envelope::ChildTranscriptFrame::Session(update));
            }
            if let Some(notification) = project_acpx_context(&ev.kind) {
                frames.push(houyicoder_protocol::envelope::ChildTranscriptFrame::Acpx(
                    notification,
                ));
            }
        }
        frames
    }

    /// Drain durable events appended since the last push: snapshot the
    /// trajectory, skip the already-pushed prefix, push each new event,
    /// advance the cursor. Shared by the post-resolve outer loop, the
    /// serve-start replay, and the mid-run notify branch so a tool-call
    /// frame ships while the run is still in flight. Idempotent — a spurious
    /// wake re-runs the cursor with nothing new to skip; never assert "new
    /// events must exist" on wake.
    pub(super) async fn push_new_events(&mut self, io: &mut ServerIo) -> Result<(), WireError> {
        let events = self.runner.store().trajectory_snapshot(self.session);
        for ev in events.iter().skip(self.pushed_count) {
            self.push_turn_event(io, ev).await?;
        }
        self.pushed_count = events.len();
        Ok(())
    }

    /// Send an event on the monotonic seq stream. Each event gets the next
    /// seq so a reconnecting client can resume from the last it processed.
    /// The seq is minted from the shared atomic the live delta sink also
    /// draws from, so live deltas and durable events share one stream.
    pub(super) async fn send_event(
        &mut self,
        io: &mut ServerIo,
        kind: FrontendEventKind,
    ) -> Result<(), WireError> {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let frame = ServerFrame::Event(EventEnvelope::new(EventSeq(seq), kind));
        self.send_typed(io, &frame).await
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

    /// Send a wire error as a response with no correlation. Used when a frame
    /// could not be paired to a req_id (bad framing) so the host still learns
    /// the failure.
    pub(super) async fn send_wire_error(
        &mut self,
        io: &mut ServerIo,
        err: WireError,
    ) -> Result<(), WireError> {
        // Correlation unknown: use a sentinel req_id the client treats as
        // out-of-band. The client logs it rather than pairing to a request.
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

/// Map a frame encoding failure to a wire error at the boundary.
fn frame_to_wire(e: FrameError) -> WireError {
    WireError::new(
        houyicoder_protocol::wire::WireErrorKind::InvalidFrame,
        e.to_string(),
        false,
    )
}
