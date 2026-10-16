//! Pins what the inbound entry admits: a frame carrying the trailing
//! terminator reads as its content, a clean half-close reads as None, and a
//! frame above the protocol's per-message byte bound reads as a size refusal
//! rather than as either.

use super::*;
use futures::SinkExt;
use futures::channel::mpsc;
use houyicoder_protocol::framing::MAX_MESSAGE_BYTES;

/// A carrier whose inbound side the returned sender feeds. The outbound side
/// is closed, so nothing this unit sends can arrive anywhere.
fn carrier() -> (FrameCarrier, mpsc::Sender<String>) {
    let (outbound_tx, outbound) = mpsc::channel::<String>(1);
    drop(outbound);
    let (inbound_tx, inbound) = mpsc::channel::<String>(4);
    (FrameCarrier::new(outbound_tx, inbound), inbound_tx)
}

#[tokio::test]
async fn test_frame_read_strips_terminator() {
    let (mut io, mut inbound) = carrier();
    inbound
        .send("hello\n".to_string())
        .await
        .expect("handed to the carrier");
    let frame = io.next_frame().await.expect("read").expect("a frame");
    assert_eq!(frame, "hello", "the terminator is not frame content");
}

#[tokio::test]
async fn test_clean_close_reads_none() {
    let (mut io, inbound) = carrier();
    drop(inbound);
    let frame = io.next_frame().await.expect("a close is not a failure");
    assert_eq!(frame, None, "a clean half-close reads as no frame");
}

#[tokio::test]
async fn test_oversized_frame_refused() {
    let (mut io, mut inbound) = carrier();
    let huge = "x".repeat(MAX_MESSAGE_BYTES + 1);
    inbound
        .send(format!("{huge}\n"))
        .await
        .expect("handed to the carrier");
    let err = io.next_frame().await.expect_err("over the bound");
    assert_eq!(
        err.category,
        ErrorCategory::MessageTooLarge,
        "a contract violation must not read as a clean close"
    );
    assert!(!err.retriable, "the same bytes fail the same way again");
}
