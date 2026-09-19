//! The reverse read of a session log: a walk over the log in reads smaller
//! than the log hands back whole lines, each once, up to the read's byte
//! budget. A line wider than that budget stays out of every read of it, and
//! a read from the log's end reports whether it reached the newest line.

use houyicoder_context::{ContextBackend, EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_memory::LocalFileBackend;
use std::path::PathBuf;

fn temp_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("houyicoder_reverse_read_{}", SessionId::new()));
    std::fs::create_dir_all(&dir).expect("create temp root");
    dir
}

fn evt(session: SessionId, id: EventId, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id,
        session,
        ts: 0,
        prev_hash: None,
        event: kind,
    }
}

fn append(b: &LocalFileBackend, s: SessionId, text: String) {
    pollster::block_on(b.append(evt(s, EventId::new(), SessionEvent::UserInput { text }))).unwrap();
}

/// Walks the log from its end in reads of one budget, newest line first, and
/// returns the lines the walk handed back with the number of reads it took.
/// Each read must move the walk to older bytes, or the walk would loop.
fn walk(b: &LocalFileBackend, s: SessionId, budget: u64) -> (Vec<(u64, String)>, usize) {
    let mut from = b.log_size(s);
    let mut seen: Vec<(u64, String)> = Vec::new();
    let mut reads = 0;
    while from > 0 {
        let r = b.read_lines_reverse(s, from, budget);
        reads += 1;
        seen.extend(r.lines);
        match r.next_from {
            Some(next) => {
                assert!(next < from, "each read covers older bytes: {next} < {from}");
                from = next;
            }
            None => break,
        }
    }
    (seen, reads)
}

/// Walking a log read by read, each on a byte budget smaller than the log,
/// returns every line once. The step a read stops on runs through a line: the
/// line it did not finish is the next read's first, because the resume point
/// it hands back sits at that line's end. A resume point inside the line left
/// its bytes behind, and the line vanished from the walk.
#[test]
fn test_reverse_walk_keeps_lines() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    // Longer than the chunk the reader reads, so a walk is several reads
    // and a chunk boundary falls inside a line.
    let body = "word ".repeat(200);
    for i in 0..200 {
        append(&b, s, format!("{i} {body}"));
    }
    // A read wider than one chunk spans several, and each internal boundary
    // falls inside a line. Those lines are the ones a walk loses when the
    // resume point skips their leftover bytes.
    for budget in [4096u64, 131_072, 262_144] {
        let (seen, _) = walk(&b, s, budget);
        assert_eq!(
            seen.len(),
            200,
            "every line comes back once at {budget}: {}",
            seen.len()
        );
        for i in 0..200 {
            let marker = format!("\"text\":\"{i} word word");
            assert_eq!(
                seen.iter().filter(|(_, t)| t.contains(&marker)).count(),
                1,
                "line {i} comes back once at {budget}, neither dropped nor repeated"
            );
        }
    }
    std::fs::remove_dir_all(&root).ok();
}

/// A read whose newest end sits on the terminator of the line the chunk
/// boundary fell inside: the read holds that whole line, so it hands it back
/// and the next read starts at its head. Starting it over instead would hand
/// the same line back forever and never reach the lines before it.
#[test]
fn test_reverse_ends_on_line() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    // A read of 64 KB holds a last line of exactly 64 KB whole: its window
    // starts on the line's head. The bytes around the text are the same for
    // every event of one kind, so measuring one line gives the text length the
    // last line needs to fill that read.
    let probe = "x".repeat(1000);
    append(&b, s, probe.clone());
    let envelope = b.log_size(s) - probe.len() as u64;
    assert!(envelope < 65536, "the line's own bytes fit in one read");
    append(&b, s, "y".repeat((65536 - envelope) as usize));
    let total = b.log_size(s);
    let r = b.read_lines_reverse(s, total, 65536);
    assert_eq!(r.lines.len(), 1, "the read holds the whole last line");
    assert!(
        r.lines[0].1.contains("yyyyyyyyyy"),
        "the line's own text: {}",
        &r.lines[0].1[..40]
    );
    let head = envelope + probe.len() as u64;
    assert_eq!(
        r.lines[0].0, head,
        "the line reports the offset it starts at"
    );
    assert!(
        r.newest_line_returned,
        "the read reached the log's newest line"
    );
    assert_eq!(r.next_from, Some(head), "the next read starts at its head");
    let rest = b.read_lines_reverse(s, head, 65536);
    assert_eq!(rest.lines.len(), 1, "the line before it: {:?}", rest.lines);
    assert!(rest.lines[0].1.contains("xxxx"), "the probe line");
    assert_eq!(rest.next_from, None, "the first line ends the walk");
    std::fs::remove_dir_all(&root).ok();
}

/// A line wider than the chunk the reader walks, as the log's last line: no
/// walk back from the end of the log reaches its head, so no part of it comes
/// back, and the read from the end reports that it stopped short of the
/// newest line. A part handed back would reach a caller as a line that is not
/// one, and one reported as the log's last line would be taken for it.
#[test]
fn test_reverse_wide_last_line() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    append(&b, s, "probe".into());
    // Wider than the 64 KB the reader walks per chunk.
    append(&b, s, "z".repeat(100_000));
    let truth = b.read_log_range(s, 0, 4096);
    let first = b.read_lines_reverse(s, b.log_size(s), 65536);
    assert!(
        first.lines.is_empty(),
        "the read stops on the wide line's bytes: {:?}",
        first.lines
    );
    assert!(
        !first.newest_line_returned,
        "the read did not reach the log's newest line"
    );
    let (seen, reads) = walk(&b, s, 65536);
    assert!(reads > 1, "passing the wide line takes more than one read");
    assert_eq!(seen.len(), 1, "only the line the walk spans comes back");
    assert_eq!(
        seen[0], truth.lines[0],
        "the line comes back as the log holds it"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// A line wider than the chunk the reader walks, as the log's first line:
/// every read below it stops inside its bytes, so no part of it ever comes
/// back, and the lines after it come back whole. A window's first segment has
/// no terminator above it, so a reader that hands it back anyway gives a
/// caller the head of that line as a line of its own.
#[test]
fn test_reverse_wide_first_line() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    append(&b, s, "z".repeat(200_008));
    append(&b, s, "small0".into());
    append(&b, s, "small1".into());
    append(&b, s, "small2".into());
    let truth = b.read_log_range(s, 0, 400_000);
    assert_eq!(truth.lines.len(), 4, "the forward read holds the whole log");
    // A read that stops inside the wide line and reaches the log's first byte
    // holds no line at all, and reports that much: a caller taking the batch's
    // first line for the log's newest would find nothing there, and the log
    // holds no newer line below the boundary either.
    let inside = b.read_lines_reverse(s, 65536, 65536);
    assert!(
        inside.lines.is_empty(),
        "no line ends below the boundary: {:?}",
        inside.lines
    );
    assert!(
        !inside.newest_line_returned,
        "a batch with no line reports reaching none"
    );
    let (seen, _) = walk(&b, s, 65536);
    assert_eq!(seen.len(), 3, "the lines after the wide one: {:?}", seen);
    let mut reversed = seen.clone();
    reversed.reverse();
    assert_eq!(
        reversed,
        truth.lines[1..].to_vec(),
        "each line comes back as the log holds it"
    );
    assert!(
        !seen.iter().any(|(_, t)| t.contains('z')),
        "no part of the wide line comes back"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// The log's trailing bytes no terminator follows: they hold no line, so the
/// newest whole line the log holds is the one before them, and a read from
/// the end reports that line as the newest it reached. Rejecting it for the
/// missing terminator instead sends a caller after a line the log never wrote.
#[test]
fn test_reverse_unterminated_tail() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    append(&b, s, "first".into());
    append(&b, s, "second".into());
    let path = root.join(format!("{s}")).join("log.jsonl");
    let size = std::fs::metadata(&path).unwrap().len();
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(size - 5).unwrap();
    let torn = std::fs::metadata(&path).unwrap().len();
    let r = b.read_lines_reverse(s, torn, 65536);
    assert_eq!(r.lines.len(), 1, "the line before the tail: {:?}", r.lines);
    assert!(
        r.lines[0].1.contains("first"),
        "the last whole line: {}",
        r.lines[0].1
    );
    assert!(
        r.newest_line_returned,
        "the tail holds no line, so the read reached the newest one"
    );
    assert_eq!(r.next_from, None, "the read reached the log's first byte");
    std::fs::remove_dir_all(&root).ok();
}

/// A read whose boundary is the terminator that ends a line: the byte at the
/// boundary closes the line the window stops on, so that line comes back whole
/// although no terminator after it is in the window, and the read reports
/// reaching the newest line at or below its boundary.
#[test]
fn test_reverse_line_end_boundary() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    append(&b, s, "first".into());
    append(&b, s, "second".into());
    let total = b.log_size(s);
    // The log's last byte is the terminator the writer puts after its last
    // line, and the line that terminator ends is the newest line of the log at
    // or below it.
    let r = b.read_lines_reverse(s, total - 1, 65536);
    assert_eq!(r.lines.len(), 2, "both lines: {:?}", r.lines.len());
    assert!(
        r.lines[0].1.contains("second"),
        "the line the boundary ends: {}",
        &r.lines[0].1[..40]
    );
    assert!(
        r.newest_line_returned,
        "the read reached the newest line at the boundary"
    );
    std::fs::remove_dir_all(&root).ok();
}
