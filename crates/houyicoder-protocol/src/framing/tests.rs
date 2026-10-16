//! Tests for this module.

use super::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Msg {
    id: u32,
    text: String,
}

#[test]
fn test_encode_ends_with_newline() {
    let m = Msg {
        id: 1,
        text: "hi".into(),
    };
    let f = encode(&m).unwrap();
    assert!(f.ends_with('\n'));
    assert_eq!(f, "{\"id\":1,\"text\":\"hi\"}\n");
}

#[test]
fn test_decoder_yields_frames_order() {
    let mut d = FrameDecoder::new(1024);
    let a = encode(&Msg {
        id: 1,
        text: "a".into(),
    })
    .unwrap();
    let b = encode(&Msg {
        id: 2,
        text: "b".into(),
    })
    .unwrap();
    // Push both frames split across two chunks (mid-frame boundary).
    let combined = format!("{a}{b}");
    d.push(combined.as_bytes()).unwrap();
    let first = d.next_frame::<Msg>().unwrap().unwrap();
    let second = d.next_frame::<Msg>().unwrap().unwrap();
    assert_eq!(first.id, 1);
    assert_eq!(second.id, 2);
    assert!(d.next_frame::<Msg>().is_none(), "no third frame");
}

#[test]
fn test_decoder_holds_until_terminator() {
    let mut d = FrameDecoder::new(1024);
    d.push(b"{\"id\":1,\"text\":\"x\"}").unwrap(); // no newline yet
    assert!(d.next_frame::<Msg>().is_none(), "partial frame not yielded");
    d.push(b"\n").unwrap();
    let m = d.next_frame::<Msg>().unwrap().unwrap();
    assert_eq!(m.id, 1);
}

#[test]
fn test_encode_refuses_oversized() {
    let m = Msg {
        id: 1,
        text: "x".repeat(MAX_MESSAGE_BYTES),
    };
    let err = encode(&m).unwrap_err();
    assert!(
        matches!(err, FrameError::TooLarge { limit, .. } if limit == MAX_MESSAGE_BYTES),
        "a message above the bound is refused before a carrier sees it: {err}"
    );
}

#[test]
fn test_frame_at_bound_admits() {
    let at_bound = "x".repeat(MAX_MESSAGE_BYTES);
    assert!(
        check_frame_size(&at_bound).is_ok(),
        "the bound itself is admissible; only a longer frame is refused"
    );
    let over = "x".repeat(MAX_MESSAGE_BYTES + 1);
    assert!(matches!(
        check_frame_size(&over),
        Err(FrameError::TooLarge { limit, actual }) if limit == MAX_MESSAGE_BYTES && actual == MAX_MESSAGE_BYTES + 1
    ));
}

#[test]
fn test_decoder_rejects_oversized_frame() {
    let mut d = FrameDecoder::new(8);
    // A frame longer than the limit without a terminator trips the guard.
    let err = d.push(b"0123456789abcdef").unwrap_err();
    assert!(matches!(err, FrameError::TooLarge { limit: 8, .. }));
}

#[test]
fn test_decoder_reports_bad_json() {
    let mut d = FrameDecoder::new(1024);
    d.push(b"not json\n").unwrap();
    let err = d.next_frame::<Msg>().unwrap().unwrap_err();
    assert!(matches!(err, FrameError::Json(_)));
}
