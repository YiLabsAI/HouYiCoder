//! The client a frontend holds to speak the wire protocol to a service. Owns
//! the connection end of a transport: performs the Hello handshake, sends
//! typed requests, and receives ServerFrames (events on the seq stream +
//! responses paired by req_id). The client tracks the highest event seq it has
//! processed so a reconnect can resume the tail. It speaks typed wire messages
//! only; it never imports engine types.

use crate::transport::Transport;
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponseEnvelope, ClientResponsePayload, RequestEnvelope, RequestId,
    ResumeFrom, ServerFrame,
};
use houyicoder_protocol::framing::{FrameError, encode};
use houyicoder_protocol::handshake::{Hello, Negotiated, negotiate};
use houyicoder_protocol::wire::{WireError, WireErrorKind};

/// A protocol client bound to one transport. One end of the connection; the
/// service holds the other. The transport is carrier mechanics only (the
/// in-memory channel for mode A, pipes for mode B), so the client is
/// carrier-agnostic.
pub struct Client {
    transport: Box<dyn Transport>,
    /// The highest event seq the client has processed, for resume on reconnect.
    resume: ResumeFrom,
    /// The count of trajectory events the client has rendered. Reported in the
    /// Hello handshake so the server skips this many events from the current
    /// trajectory snapshot (the mirror is append-only with no per-event seq
    /// the server could map a client seq to, so a count is the cursor). A fresh
    /// client reports 0 to get the whole transcript.
    events_seen: u64,
    /// The next req_id to mint. Caller-supplied ids are also accepted; the
    /// counter is a convenience for callers that do not track their own.
    next_req_id: u64,
}

impl Client {
    /// Build a client over a transport. The transport is consumed; the client
    /// owns it for the connection lifetime.
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            resume: ResumeFrom::from_start(),
            events_seen: 0,
            next_req_id: 1,
        }
    }

    /// Perform the Hello handshake. Both ends send Hello first; the client
    /// validates the service's version + capabilities. Returns the negotiated
    /// peer capabilities the client can branch on. A version mismatch fails
    /// non-retriable.
    pub async fn connect(&mut self) -> Result<Negotiated, WireError> {
        let local = Hello {
            last_event_count: Some(self.events_seen),
            ..Hello::local()
        };
        self.send_typed(&local).await?;
        let Some(frame) = self.transport.recv_frame().await.map_err(recv_to_wire)? else {
            return Err(WireError::new(
                WireErrorKind::Unavailable,
                "service closed before hello",
                false,
            ));
        };
        let peer: Hello = decode_frame(&frame)?;
        negotiate(&local, &peer)
    }

    /// Mint the next request id. Monotonic within the client; callers that
    /// prefer their own id scheme can pass it directly to send_request.
    pub fn next_request_id(&mut self) -> RequestId {
        let id = RequestId(self.next_req_id);
        self.next_req_id += 1;
        id
    }

    /// Send a request envelope. The caller mints the req_id; the matching
    /// response returns it. Fire-and-forget on the wire: the reply arrives via
    /// next_frame as a ServerFrame::Response.
    pub async fn send_request(
        &mut self,
        req_id: RequestId,
        payload: houyicoder_protocol::frontend::FrontendRequest,
    ) -> Result<(), WireError> {
        let env = RequestEnvelope::new(req_id, payload);
        self.send_typed(&ClientFrame::Request(env)).await
    }

    /// Answer a server reverse request (a ServerFrame::Request the caller
    /// received via next_frame) with the matching reverse response, paired by
    /// the same req_id the server minted.
    pub async fn send_reverse_response(
        &mut self,
        req_id: RequestId,
        payload: ClientResponsePayload,
    ) -> Result<(), WireError> {
        let env = ClientResponseEnvelope::new(req_id, payload);
        self.send_typed(&ClientFrame::Response(env)).await
    }

    /// Send a JSON-RPC notification (no id, no reply). Used for client-to-server
    /// signals like session/cancel, which the server reads mid-run without
    /// correlating a response.
    pub async fn send_notification(
        &mut self,
        notif: houyicoder_protocol::acp_wire::AcpNotification,
    ) -> Result<(), WireError> {
        self.send_typed(&notif).await
    }

    /// Receive the next server frame: an event on the seq stream or a
    /// response paired to a prior request. The client advances its resume
    /// cursor for each event so reconnect resumes from the tail.
    pub async fn next_frame(&mut self) -> Result<ServerFrame, WireError> {
        let frame = self
            .transport
            .recv_frame()
            .await
            .map_err(recv_to_wire)?
            .ok_or_else(|| WireError::new(WireErrorKind::Unavailable, "service closed", false))?;
        let decoded = decode_frame::<ServerFrame>(&frame)?;
        if let ServerFrame::Event(ev) = &decoded
            && ev.seq.0 >= self.resume.0.map(|s| s.0).unwrap_or(0)
        {
            self.resume = ResumeFrom::after(ev.seq);
            self.events_seen = self.events_seen.saturating_add(1);
        }
        Ok(decoded)
    }

    /// The resume cursor: the last event seq processed, to report on reconnect.
    pub fn resume_cursor(&self) -> ResumeFrom {
        self.resume
    }

    /// Encode a typed message and send it as one frame.
    async fn send_typed<T: serde::Serialize>(&mut self, msg: &T) -> Result<(), WireError> {
        let frame = encode(msg).map_err(frame_to_wire)?;
        self.transport.send_frame(&frame).await
    }
}

/// Decode a frame string into a typed message. Surfaces bad framing as a wire
/// error at the boundary.
fn decode_frame<T: serde::de::DeserializeOwned>(frame: &str) -> Result<T, WireError> {
    serde_json::from_str(frame)
        .map_err(|e| WireError::new(WireErrorKind::InvalidFrame, e.to_string(), false))
}

fn frame_to_wire(e: FrameError) -> WireError {
    WireError::new(WireErrorKind::InvalidFrame, e.to_string(), false)
}

fn recv_to_wire(e: WireError) -> WireError {
    e
}
