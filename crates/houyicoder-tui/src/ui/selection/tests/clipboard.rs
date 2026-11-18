//! Clipboard queue and OSC 52 sequence tests. The platform's outcome
//! decision is tested there; this file covers the tui-owned pieces: the
//! result queue a background write reports through, the async boundary, and
//! the OSC 52 sequence builder.

#![cfg(test)]

use std::io::{Error, Result};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};

use super::super::clipboard::{
    ClipboardWriter, CopyReceipt, CopyReport, CopyResultQueue, RecordingClipboard, SystemClipboard,
    finish_copy, osc_sequence, raw_osc52,
};
use houyicoder_platform::desktop::clipboard::CopyOutcome;

/// A receipt sent through the queue waits for the drain: the send happens
/// before any drain, and the first drain reads it.
#[test]
fn test_queue_delivers_receipt() {
    let queue = CopyResultQueue::new();
    queue.send(CopyReceipt {
        chars: 5,
        outcome: CopyReport::NativeCopied,
    });
    let drained = queue.drain();
    assert_eq!(drained.len(), 1, "the receipt is delivered");
    assert_eq!(drained[0].chars, 5);
    assert_eq!(drained[0].outcome, CopyReport::NativeCopied);
    assert!(queue.drain().is_empty(), "the drain consumed it");
}

/// A receipt that lands between two drains is read by the next one, not
/// lost and not duplicated.
#[test]
fn test_queue_holds_between_drains() {
    let queue = CopyResultQueue::new();
    assert!(queue.drain().is_empty(), "an empty queue drains nothing");
    queue.send(CopyReceipt {
        chars: 2,
        outcome: CopyReport::TerminalRequested,
    });
    let drained = queue.drain();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].chars, 2);
}

/// The recording clipboard reports a native success synchronously and
/// records the exact text, so selection tests can assert what a gesture
/// would copy without touching the OS clipboard.
#[test]
fn test_recording_clipboard_reports() {
    let clipboard = RecordingClipboard::default();
    let queue = CopyResultQueue::new();
    clipboard.write("copied text", &queue);
    assert_eq!(
        clipboard.captured.lock().expect("captured").as_slice(),
        ["copied text"],
        "the exact text is recorded"
    );
    let drained = queue.drain();
    let receipt = drained.first().expect("the receipt is queued");
    assert_eq!(receipt.chars, "copied text".chars().count());
    assert_eq!(receipt.outcome, CopyReport::NativeCopied);
}

/// The writer starts its copy off the calling thread: the call returns with
/// the receipt not yet sent, and the receipt arrives from the background
/// thread with the outcome that body produced. The body is injected, so the
/// test never touches a real clipboard.
#[test]
fn test_system_clipboard_writes_async() {
    let (started_tx, started_rx): (Sender<()>, Receiver<()>) = channel();
    let (block_tx, block_rx) = channel::<()>();
    let block_rx = Arc::new(Mutex::new(block_rx));
    let clipboard = SystemClipboard::with_copy(move |_text| {
        let _ = started_tx.send(());
        // Hold the copy until the test observed the write returned.
        let receiver = block_rx.lock().expect("block receiver");
        let _ = receiver.recv();
        CopyReport::NativeCopied
    });
    let queue = CopyResultQueue::new();
    clipboard.write("async probe", &queue);
    assert!(
        queue.drain().is_empty(),
        "the write call returns before the copy completes"
    );
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("the background copy starts");
    block_tx.send(()).expect("the copy is released");
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut receipt = None;
    while Instant::now() < deadline {
        if let Some(r) = queue.drain().into_iter().next() {
            receipt = Some(r);
            break;
        }
        sleep(Duration::from_millis(1));
    }
    let receipt = receipt.expect("the background write reports");
    assert_eq!(receipt.chars, "async probe".chars().count());
    assert_eq!(receipt.outcome, CopyReport::NativeCopied);
}

/// A copy whose body reports a failure carries that outcome to the queue
/// unchanged, so the notice is built from what happened rather than from
/// what was attempted.
#[test]
fn test_system_clipboard_keeps_failure() {
    let clipboard = SystemClipboard::with_copy(|_| CopyReport::CopyFailed);
    let queue = CopyResultQueue::new();
    clipboard.write("doomed", &queue);
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut receipt = None;
    while Instant::now() < deadline {
        if let Some(r) = queue.drain().into_iter().next() {
            receipt = Some(r);
            break;
        }
        sleep(Duration::from_millis(1));
    }
    assert_eq!(
        receipt.expect("the background write reports").outcome,
        CopyReport::CopyFailed
    );
}

/// The terminal escape is written on exactly the outcomes with no native
/// delivery, and its failure turns the report red rather than leaving a
/// claim of a request that never left the process.
#[test]
fn test_fallback_writes_escape() {
    let escapes = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&escapes);
    let emit = move |seq: &str| {
        sink.lock().expect("escape sink").push(seq.to_string());
        Ok(())
    };

    let failed = finish_copy(CopyOutcome::NativeFailed, "hi", &emit);
    assert_eq!(failed, CopyReport::NativeFailedTerminalRequested);
    let unavailable = finish_copy(CopyOutcome::NoNativePath, "hi", &emit);
    assert_eq!(unavailable, CopyReport::TerminalRequested);
    assert_eq!(
        escapes.lock().expect("escape sink").len(),
        2,
        "both fallback outcomes write one escape each"
    );
    assert!(
        escapes.lock().expect("escape sink")[0].contains("]52;c;"),
        "the escape is an OSC 52 sequence"
    );

    let copied = finish_copy(CopyOutcome::NativeCopied, "hi", |_| Ok(()));
    assert_eq!(copied, CopyReport::NativeCopied);
    let buffered = finish_copy(CopyOutcome::TmuxBufferCopied, "hi", |_| Ok(()));
    assert_eq!(buffered, CopyReport::TmuxBufferCopied);
}

/// A dead terminal makes the fallback a total failure, so the notice says
/// the copy failed instead of requesting a copy nobody will see.
#[test]
fn test_dead_escape_fails() {
    let dead = |_seq: &str| -> Result<()> { Err(Error::other("stdout is gone")) };
    assert_eq!(
        finish_copy(CopyOutcome::NativeFailed, "hi", dead),
        CopyReport::CopyFailed
    );
    assert_eq!(
        finish_copy(CopyOutcome::NoNativePath, "hi", dead),
        CopyReport::CopyFailed
    );
}

/// Raw OSC 52 uses BEL, not ST, for wider terminal support.
#[test]
fn test_raw_osc52_bel_terminator() {
    let seq = raw_osc52("aGVsbG8=");
    assert_eq!(seq, "\x1b]52;c;aGVsbG8=\x07");
}

/// Inside tmux the same copy is wrapped in the passthrough envelope with the
/// inner escape doubled, or the multiplexer would eat the sequence and the
/// outer terminal would never see it.
#[test]
fn test_tmux_wraps_osc52() {
    let seq = osc_sequence("aGVsbG8=", true);
    assert!(seq.starts_with("\x1bPtmux;"), "missing DCS opener: {seq:?}");
    assert!(
        seq.contains("\x1b\x1b]52;c;aGVsbG8=\x07"),
        "inner escape is doubled: {seq:?}"
    );
    assert!(seq.ends_with("\x1b\\"), "missing ST closer: {seq:?}");
    assert_eq!(
        osc_sequence("aGVsbG8=", false),
        raw_osc52("aGVsbG8="),
        "outside tmux the raw form is sent"
    );
}

/// The b64 step must use standard padded base64.
#[test]
fn test_osc52_b64_encoding() {
    let b64 = STANDARD.encode(b"hello");
    assert_eq!(b64, "aGVsbG8=");
    let seq = raw_osc52(&b64);
    assert_eq!(seq, format!("\x1b]52;c;{b64}\x07"));
}
