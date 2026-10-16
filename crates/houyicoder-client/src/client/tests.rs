//! Pins what the decode entry admits: a typed frame within the protocol's
//! per-message byte bound decodes, one above it is refused for its size, and a
//! malformed one stays a malformed-frame failure.

use super::*;
use houyicoder_protocol::envelope::EventEnvelope;
use houyicoder_protocol::envelope::EventSeq;
use houyicoder_protocol::framing::MAX_MESSAGE_BYTES;
use houyicoder_protocol::frontend::FrontendEvent;

#[test]
fn test_decode_reads_typed_frame() {
    let line = encode(&ServerFrame::Event(EventEnvelope::new(
        EventSeq(1),
        FrontendEvent::Message { delta: "hi".into() },
    )))
    .expect("encode");
    let decoded: ServerFrame = decode_frame(line.trim_end()).expect("decodes");
    let ServerFrame::Event(env) = decoded else {
        panic!("the event frame round-trips as an event");
    };
    assert_eq!(env.seq, EventSeq(1));
}

#[test]
fn test_decode_refuses_oversized() {
    let huge = "x".repeat(MAX_MESSAGE_BYTES + 1);
    let err = decode_frame::<ServerFrame>(&huge).expect_err("over the bound");
    assert_eq!(err.category, ErrorCategory::MessageTooLarge);
    assert!(!err.retriable, "the same bytes fail the same way again");
}

#[test]
fn test_malformed_frame_distinct() {
    let err = decode_frame::<ServerFrame>("{").expect_err("not a frame");
    assert_eq!(
        err.category,
        ErrorCategory::InvalidFrame,
        "a malformed frame is not a size refusal"
    );
}
