//! Tests for this module.

use crate::capture::CaptureStop;
use crate::capture::CaptureStopReason;

use super::*;

#[test]
fn test_tail_keeps_last_lines() {
    let tail = CommandTail::new();
    tail.push_bytes(b"one\ntwo\nthree\nfour\nfive\nsix\nseven\n");
    assert_eq!(
        tail.snapshot(),
        vec!["three", "four", "five", "six", "seven"],
        "the window holds the most recent lines"
    );
}

#[test]
fn test_tail_joins_split_chunks() {
    let tail = CommandTail::new();
    tail.push_bytes(b"hel");
    tail.push_bytes(b"lo\nwor");
    tail.push_bytes(b"ld\n");
    assert_eq!(tail.snapshot(), vec!["hello", "world"]);
}

#[test]
fn test_tail_shortens_long_lines() {
    let tail = CommandTail::new();
    tail.push_bytes(format!("{}\n", "x".repeat(TAIL_LINE_CHARS + 50)).as_bytes());
    let lines = tail.snapshot();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].chars().count(), TAIL_LINE_CHARS);
}

/// A line still being written, with no newline yet, is shortened to the same
/// cap as a finished one: a snapshot never hands the row a line longer than it
/// can show.
#[test]
fn test_tail_shortens_partial_line() {
    let tail = CommandTail::new();
    tail.push_bytes("x".repeat(TAIL_LINE_CHARS + 80).as_bytes());
    let lines = tail.snapshot();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].chars().count(), TAIL_LINE_CHARS);
}

#[test]
fn test_tail_shows_partial_line() {
    let tail = CommandTail::new();
    tail.push_bytes(b"first\nstill writing");
    assert_eq!(tail.snapshot(), vec!["first", "still writing"]);
}

/// A codepoint read across two chunks is held until its bytes are whole, so
/// the window shows the character instead of a replacement mark.
#[test]
fn test_tail_waits_for_codepoint() {
    let tail = CommandTail::new();
    let snowman = "\u{2603}".as_bytes();
    tail.push_bytes(&snowman[..2]);
    assert!(
        tail.snapshot().is_empty(),
        "half a codepoint is not shown as anything"
    );
    tail.push_bytes(&snowman[2..]);
    assert_eq!(tail.snapshot(), vec!["\u{2603}"]);
}

/// A byte that cannot start a codepoint is replaced and skipped, so the text
/// after it still reaches the window.
#[test]
fn test_tail_replaces_bad_byte() {
    let tail = CommandTail::new();
    tail.push_bytes(b"ok \xff done\nnext\n");
    assert_eq!(tail.snapshot(), vec!["ok \u{fffd} done", "next"]);
}

/// A receiver that records what it saw and answers with a fixed verdict, so
/// the test can prove the tail neither swallows nor duplicates a chunk.
struct RecordingReceiver {
    seen: Mutex<Vec<(CapturedStream, Vec<u8>)>>,
    verdict: CaptureVerdict,
}

impl RawOutputReceiver for RecordingReceiver {
    fn receive(&self, stream: CapturedStream, bytes: &[u8]) -> CaptureVerdict {
        self.seen
            .lock()
            .expect("recording lock")
            .push((stream, bytes.to_vec()));
        self.verdict.clone()
    }
}

#[test]
fn test_tailing_passes_chunks() {
    let capture = Arc::new(RecordingReceiver {
        seen: Mutex::new(Vec::new()),
        verdict: CaptureVerdict::Continue,
    });
    let tail = CommandTail::new();
    let tee = TailingReceiver::new(tail.clone(), Some(capture.clone()));
    assert_eq!(
        tee.receive(CapturedStream::Stdout, b"hello\n"),
        CaptureVerdict::Continue
    );
    assert_eq!(tail.snapshot(), vec!["hello"]);
    let seen = capture.seen.lock().expect("recording lock");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, CapturedStream::Stdout);
    assert_eq!(seen[0].1, b"hello\n");
}

#[test]
fn test_tailing_keeps_stop_verdict() {
    let stop = CaptureVerdict::Stop(CaptureStop {
        reason: CaptureStopReason::ByteBudget,
        stdout_bytes: 10,
        stderr_bytes: 0,
    });
    let capture = Arc::new(RecordingReceiver {
        seen: Mutex::new(Vec::new()),
        verdict: stop.clone(),
    });
    let tee = TailingReceiver::new(CommandTail::new(), Some(capture));
    assert_eq!(
        tee.receive(CapturedStream::Stdout, b"x"),
        stop,
        "the tail must not hide a stop the capture asked for"
    );
}

#[test]
fn test_tailing_without_capture() {
    let tee = TailingReceiver::new(CommandTail::new(), None);
    assert_eq!(
        tee.receive(CapturedStream::Stderr, b"note\n"),
        CaptureVerdict::Continue
    );
}
