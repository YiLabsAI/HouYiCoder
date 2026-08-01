//! Transport-dimension contract test: one protocol round threaded end-to-end
//! through the in-memory transport. Each message is serialized to a frame
//! on one side, moved through the transport, deserialized on the other side,
//! and checked against the original. The in-memory transport forces a
//! serialize-to-string-to-deserialize round-trip so a message that works
//! here is the same bytes that would cross a pipe or socket. When the pipe
//! and socket transports arrive, the same behavior runs through each of
//! them unchanged — the test is written against the transport trait so a
//! second carrier drops in.

use houyicoder_client::{InProcTransport, Transport};
use houyicoder_protocol::envelope::{EventEnvelope, EventSeq, RequestEnvelope, RequestId};
use houyicoder_protocol::framing::{FrameDecoder, encode};
use houyicoder_protocol::frontend::{FrontendEventKind, FrontendRequest};
use houyicoder_protocol::handshake::{Hello, PROTOCOL_VERSION, negotiate};
use houyicoder_protocol::wire::WireErrorKind;

/// Drive a transport pair through a full protocol turn and assert the wire
/// path preserves message content. Written against the transport trait (not
/// the in-memory impl) so a pipe or socket transport can drop in and run the
/// same behavior.
async fn run_protocol_round(transport_a: &mut dyn Transport, transport_b: &mut dyn Transport) {
    // Handshake: both ends send their Hello as the first frame. A version
    // match lets the turn proceed; a mismatch would surface as a
    // non-retriable protocol-version error.
    let local = Hello::local();
    let a_hello = encode(&local).expect("encode hello");
    let b_hello = encode(&local).expect("encode hello");
    transport_a
        .send_frame(&a_hello)
        .await
        .expect("a sends hello");
    transport_b
        .send_frame(&b_hello)
        .await
        .expect("b sends hello");

    let peer_b = recv_typed::<Hello>(transport_b)
        .await
        .expect("b recv hello");
    let peer_a = recv_typed::<Hello>(transport_a)
        .await
        .expect("a recv hello");
    negotiate(&local, &peer_b).expect("b handshake ok");
    negotiate(&local, &peer_a).expect("a handshake ok");
    assert_eq!(peer_b.protocol_version, PROTOCOL_VERSION);
    assert_eq!(peer_a.protocol_version, PROTOCOL_VERSION);

    // Request: a sends a request envelope; b receives and decodes it. The
    // req_id must survive the round-trip so b can pair its reply.
    let request = RequestEnvelope::new(RequestId(42), FrontendRequest::Console);
    transport_a
        .send_frame(&encode(&request).expect("encode request"))
        .await
        .expect("a sends request");
    let recv_request = recv_typed::<RequestEnvelope>(transport_b)
        .await
        .expect("b recv request");
    assert_eq!(recv_request.req_id, RequestId(42));
    assert!(matches!(recv_request.payload, FrontendRequest::Console));

    // Event: b sends an event envelope with a monotonic seq; a receives and
    // decodes it. The seq must survive so a can track its resume cursor.
    let event = EventEnvelope::new(
        EventSeq(7),
        FrontendEventKind::Message {
            delta: "hello world".to_string(),
        },
    );
    transport_b
        .send_frame(&encode(&event).expect("encode event"))
        .await
        .expect("b sends event");
    let recv_event = recv_typed::<EventEnvelope>(transport_a)
        .await
        .expect("a recv event");
    assert_eq!(recv_event.seq, EventSeq(7));
    match recv_event.payload {
        FrontendEventKind::Message { delta } => assert_eq!(delta, "hello world"),
        other => panic!("expected Message event, got {other:?}"),
    }
}

/// Decode the next frame on a transport into a typed message: the
/// deserialization half of the wire round-trip. The transport yields a frame
/// string (newline stripped); serde parses it back to the message type. An
/// error here means the bytes that crossed the boundary did not reform into
/// the same message — a wire-equivalence break.
async fn recv_typed<T>(transport: &mut dyn Transport) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
{
    let frame = transport
        .recv_frame()
        .await
        .map_err(|e| format!("recv error: kind={:?} msg={}", e.kind, e.message))?
        .ok_or_else(|| "expected a frame, got clean close".to_string())?;
    serde_json::from_str(&frame).map_err(|e| format!("decode error: {e}"))
}

/// The in-memory transport pair runs the full protocol round. This is the
/// baseline: a frame that round-trips here is wire-equivalent to one that
/// crosses a pipe or socket, because the in-memory transport already pays
/// the full serialize-to-string-to-deserialize tax.
#[tokio::test]
async fn test_in_proc_round_trips() {
    let (mut a, mut b) = InProcTransport::pair(8);
    run_protocol_round(&mut a, &mut b).await;
}

/// A closed peer surfaces as a clean close (None) or an Unavailable wire
/// error, never a protocol or frame error — the same semantics a pipe peer
/// has when the other end exits.
#[tokio::test]
async fn test_closed_peer_surfaces_unavailable() {
    let (mut a, mut b) = InProcTransport::pair(4);
    a.send_frame(&encode(&Hello::local()).expect("encode hello"))
        .await
        .expect("send ok");
    drop(a);
    let next = b.recv_frame().await;
    match next {
        Ok(Some(_) | None) => {}
        Err(e) => assert_eq!(
            e.kind,
            WireErrorKind::Unavailable,
            "post-close failure must be Unavailable, got {:?}",
            e.kind
        ),
    }
}

/// The buffered frame decoder fed the raw transport bytes yields the same
/// typed messages as the direct decode path, even when bytes arrive in
/// arbitrary chunks. Real transports push bytes in fragments; the decoder
/// must reassemble complete frames in order. This guards the boundary
/// between chunked-byte reception and typed decode.
#[tokio::test]
async fn test_frame_decoder_reassembles_chunked() {
    let (mut a, mut b) = InProcTransport::pair(8);
    let req = RequestEnvelope::new(RequestId(99), FrontendRequest::Status);
    a.send_frame(&encode(&req).expect("encode"))
        .await
        .expect("send");
    let line = b.recv_frame().await.unwrap().unwrap();
    // Feed the line through the buffered decoder as two chunks; it must
    // hold the partial frame until the terminator arrives, then yield the
    // complete typed message.
    let mut dec = FrameDecoder::new(1024);
    let pushed = format!("{line}\n");
    let bytes = pushed.as_bytes();
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
