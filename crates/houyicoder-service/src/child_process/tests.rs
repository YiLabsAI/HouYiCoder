//! Tests for this module.

use std::io::Cursor;
use std::io::sink;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

use super::*;

/// The tail keeps only the newest lines: a child that keeps talking cannot
/// grow the hold, and the newest failure is the one still visible.
#[test]
fn test_tail_keeps_newest() {
    let tail = StderrTail::default();
    for n in 0..STDERR_TAIL_LINES + 8 {
        tail.push(format!("line {n}"));
    }
    let lines = tail.lines();
    assert_eq!(lines.len(), STDERR_TAIL_LINES);
    assert_eq!(lines.first().map(String::as_str), Some("line 8"));
    assert_eq!(
        lines.last().map(String::as_str),
        Some(format!("line {}", STDERR_TAIL_LINES + 7).as_str())
    );
}

/// A line past the character cap is cut at a character boundary: a multi-byte
/// character is never split into invalid text.
#[test]
fn test_tail_cuts_wide_line() {
    let tail = StderrTail::default();
    let line: String = "进".repeat(STDERR_LINE_CHARS + 50);
    tail.push(line);
    let kept = &tail.lines()[0];
    assert_eq!(kept.chars().count(), STDERR_LINE_CHARS);
    assert!(kept.is_char_boundary(kept.len()));
}

/// The channels deliver the child's output to the reader half and the writer
/// half's bytes to the child's input.
#[tokio::test]
async fn test_channels_move_bytes() {
    struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipes = Pipes {
        stdin: Box::new(Recorder(seen.clone())),
        stdout: Box::new(Cursor::new(b"from child".to_vec())),
        stderr: None,
    };
    let (mut channels, _tail) = start(pipes);
    let mut out = Vec::new();
    channels
        .reader
        .read_to_end(&mut out)
        .await
        .expect("child output arrives");
    assert_eq!(out, b"from child");
    channels
        .writer
        .write_all(b"to child")
        .await
        .expect("write reaches the channels");
    channels.writer.flush().await.expect("flush");
    // The writer thread flushes on its own turn; poll for the bytes rather
    // than assuming the same instant.
    let mut delivered = false;
    for _ in 0..100 {
        if seen.lock().expect("lock").as_slice() == b"to child" {
            delivered = true;
            break;
        }
        sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(delivered, "the child's input received the bytes");
}

/// A stderr line without a trailing newline still lands in the tail when the
/// stream ends.
#[tokio::test]
async fn test_tail_reads_partial_line() {
    let pipes = Pipes {
        stdin: Box::new(sink()),
        stdout: Box::new(Cursor::new(Vec::new())),
        stderr: Some(Box::new(Cursor::new(b"last words".to_vec()))),
    };
    let (_channels, tail) = start(pipes);
    let mut lines = Vec::new();
    for _ in 0..100 {
        lines = tail.lines();
        if !lines.is_empty() {
            break;
        }
        sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(lines, vec!["last words"]);
}

/// A URI escapes the bytes a server would otherwise re-spell: a percentage
/// sign, a space, and a non-ASCII name all decode back to the same path.
#[test]
fn test_file_uri_escapes_bytes() {
    assert_eq!(
        file_uri(Path::new("/work/a b#c?.rs")),
        "file:///work/a%20b%23c%3F.rs"
    );
    assert_eq!(
        file_uri(Path::new("/work/100%.rs")),
        "file:///work/100%25.rs"
    );
    let non_ascii = file_uri(Path::new("/work/\u{6d4b}\u{8bd5}.rs"));
    assert_eq!(non_ascii, "file:///work/%E6%B5%8B%E8%AF%95.rs");
}

/// A file URI decodes back to the path it was built from, so a caller that
/// keys a store by the decoded path and a server that publishes the encoded
/// spelling meet on one value.
#[test]
fn test_uri_round_trip() {
    for text in [
        "/work/a b#c?.rs",
        "/work/100%.rs",
        "/work/\u{6d4b}\u{8bd5}.rs",
    ] {
        let uri = file_uri(Path::new(text));
        assert_eq!(
            path_from_uri(&uri).as_deref(),
            Some(Path::new(text)),
            "{uri}"
        );
    }
    assert!(path_from_uri("https://example.test/x").is_none());
}
