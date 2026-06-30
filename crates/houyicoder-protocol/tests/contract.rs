//! Contract test skeleton: the wire types round-trip bit-equivalently through
//! the framing path. Every message the protocol defines serializes to a frame
//! and deserializes back to the same typed value, so a peer re-encoding a
//! message it received produces the same bytes. The transport dimension
//! (threading these frames through the in-memory, pipe, and socket carriers)
//! is added when those transports arrive, so the same behavior runs through
//! each carrier and asserts wire equivalence end-to-end. Here the round-trip
//! is serialization only — the wire contract surface.

use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponseEnvelope, ClientResponsePayload, EventEnvelope, EventSeq,
    RequestEnvelope, RequestId, ResumeFrom, ServerFrame, ServerRequestEnvelope,
    ServerRequestPayload,
};
use houyicoder_protocol::framing::{FrameDecoder, encode};
use houyicoder_protocol::frontend::{
    FrontendEventKind, FrontendRequest, SessionId,
    run::{ApprovalDecision, ApprovalRequest, ContentBlock},
};
use houyicoder_protocol::handshake::{Hello, PROTOCOL_VERSION, negotiate};
use houyicoder_protocol::wire::{WireError, WireErrorKind};

fn sample_approval_request() -> ApprovalRequest {
    ApprovalRequest {
        call_id: "toolu_1".into(),
        tool_name: "bash".into(),
        input: serde_json::Value::Null,
        options: Vec::new(),
        reason: None,
    }
}

fn sample_approval_decision() -> ApprovalDecision {
    ApprovalDecision {
        call_id: "toolu_1".into(),
        approved: true,
        updated_input: None,
        scope: "once".into(),
    }
}

/// Assert a message survives a serialize-to-frame-to-deserialize round-trip
/// with no loss: the frame is the bytes a carrier would move; decoding it
/// back and re-encoding must produce the identical frame, so a peer that
/// re-encodes a received message emits the same bytes. Compared as bytes
/// (not typed equality) so the assertion is the actual wire-equivalence
/// property and no public type needs to derive value equality for the test.
fn round_trip<T>(label: &str, msg: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let frame1 = encode(msg).expect("encode");
    assert!(
        frame1.ends_with('\n'),
        "{label}: frame must end with newline"
    );
    let back: T = serde_json::from_str(frame1.strip_suffix('\n').unwrap()).expect("decode");
    let frame2 = encode(&back).expect("re-encode");
    assert_eq!(frame1, frame2, "{label}: re-encode must be bit-identical");
}

/// The Hello frame round-trips, and a matching peer negotiates cleanly. A
/// version mismatch fails the handshake as a non-retriable protocol-version
/// error rather than letting a peer enter a half-working session.
#[test]
fn test_hello_round_trips_negotiates() {
    let local = Hello::local();
    round_trip("hello", &local);

    let peer = Hello::local();
    let negotiated = negotiate(&local, &peer).expect("same version negotiates");
    assert_eq!(negotiated.peer_capabilities, local.capabilities);
    assert_eq!(local.protocol_version, PROTOCOL_VERSION);

    // A version mismatch is a hard, non-retriable failure.
    let wrong = Hello {
        protocol_version: PROTOCOL_VERSION + 1,
        capabilities: local.capabilities.clone(),
        last_event_count: None,
    };
    let err = negotiate(&local, &wrong).expect_err("mismatch must fail");
    assert_eq!(err.kind, WireErrorKind::ProtocolVersion);
    assert!(!err.retriable, "version mismatch is not retriable");
}

/// A request envelope round-trips with its req_id intact — the value the
/// response pairs its reply on. Every request variant that the daemon must
/// dispatch must survive this path.
#[test]
fn test_request_envelope_round_trips() {
    let req = RequestEnvelope::new(RequestId(42), FrontendRequest::Console);
    round_trip("request-console", &req);
    assert_eq!(req.req_id, RequestId(42));
    assert!(matches!(req.payload, FrontendRequest::Console));

    // A request carrying a content payload must preserve it verbatim.
    let with_text = RequestEnvelope::new(
        RequestId(7),
        FrontendRequest::MessageSend {
            session_id: SessionId::new("sess-1"),
            content: vec![ContentBlock::Text {
                text: "hello world".to_string(),
            }],
        },
    );
    round_trip("request-message", &with_text);

    // A client frame tags the request the server decodes, and a reverse
    // response the client sends to answer a server reverse request.
    let cf_req = ClientFrame::Request(with_text.clone());
    round_trip("client-frame-request", &cf_req);
    let cf_res = ClientFrame::Response(ClientResponseEnvelope::new(
        RequestId(5),
        ClientResponsePayload::Permission(sample_approval_decision()),
    ));
    round_trip("client-frame-response", &cf_res);
    let sf_req = ServerFrame::Request(ServerRequestEnvelope::new(
        RequestId(5),
        ServerRequestPayload::Permission(sample_approval_request()),
    ));
    round_trip("server-frame-request", &sf_req);
}

/// An event envelope round-trips with its monotonic seq intact — the value
/// the client tracks for resume. Every event variant the daemon pushes must
/// survive this path.
#[test]
fn test_event_envelope_round_trips() {
    let evt = EventEnvelope::new(
        EventSeq(7),
        FrontendEventKind::Message {
            delta: "hello world".to_string(),
        },
    );
    round_trip("event-message", &evt);
    assert_eq!(evt.seq, EventSeq(7));
}

/// The resume cursor round-trips in both its forms: a fresh attach (replay
/// from the start) encodes null, and a resume-after encodes the seq.
#[test]
fn test_resume_cursor_round_trips() {
    let from_start = ResumeFrom::from_start();
    let after = ResumeFrom::after(EventSeq(3));
    round_trip("resume-from-start", &from_start);
    round_trip("resume-after", &after);

    let start_json = serde_json::to_string(&from_start).unwrap();
    assert!(start_json.contains("null"), "from_start encodes null");
    let after_json = serde_json::to_string(&after).unwrap();
    assert!(after_json.contains("3"), "after encodes the seq");
}

/// The wire error round-trips with its kind, retriable flag, and correlation.
/// A peer that receives an error must be able to branch on kind and retry
/// per the retriable hint. Compared field-by-field because the wire error
/// carries a message string and does not derive value equality.
#[test]
fn test_wire_error_round_trips() {
    let err =
        WireError::new(WireErrorKind::InvalidFrame, "bad json", true).with_correlation("req-1");
    let frame = encode(&err).expect("encode");
    let back: WireError = serde_json::from_str(frame.strip_suffix('\n').unwrap()).expect("decode");
    assert_eq!(back.kind, WireErrorKind::InvalidFrame);
    assert_eq!(back.message, "bad json");
    assert!(back.retriable);
    assert_eq!(back.correlation.as_deref(), Some("req-1"));
}

/// The buffered frame decoder yields the same typed messages a direct
/// decode produces, even when bytes arrive in arbitrary chunks. Real
/// transports push bytes in fragments; the decoder must reassemble complete
/// frames in order. This guards the boundary between chunked-byte reception
/// and typed decode.
#[test]
fn test_frame_decoder_reassembles_input() {
    let req = RequestEnvelope::new(RequestId(99), FrontendRequest::Status);
    let frame = encode(&req).expect("encode");
    let bytes = frame.as_bytes();

    let mut dec = FrameDecoder::new(1024);
    // Push the frame in two halves; the decoder must hold the partial frame
    // until the terminator arrives, then yield the complete message.
    let mid = bytes.len() / 2;
    dec.push(&bytes[..mid]).expect("push first half");
    assert!(
        dec.next_frame::<RequestEnvelope>().is_none(),
        "no frame yet"
    );
    dec.push(&bytes[mid..]).expect("push second half");
    let decoded = dec
        .next_frame::<RequestEnvelope>()
        .expect("a frame")
        .expect("parsed");
    assert_eq!(decoded.req_id, RequestId(99));
    assert!(matches!(decoded.payload, FrontendRequest::Status));
}
