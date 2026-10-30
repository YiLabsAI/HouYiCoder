//! Tests for the catalog's paging vocabulary: the encoded form of a request,
//! and the values a reader gets back.

use super::*;
use serde_json::json;

fn entry() -> ResultEntry {
    ResultEntry {
        id: ResultId::new("record-1:stdout"),
        tool: Some("bash".into()),
        command: Some("cargo test".into()),
        event: "event-1".into(),
        call: Some("call-1".into()),
        created_ms: Some(1_700_000_000_000),
        exit: Some(0),
        completeness: ResultCompleteness::Complete,
        stop_reason: None,
        stream: ResultStream::Stdout,
        encoding: ResultEncoding::Raw,
        bytes: Some(2048),
    }
}

/// A reader echoes the name it was issued, and the name is one scalar in its
/// encoded form rather than an object the caller has to rebuild.
#[test]
fn test_result_id_round_trips() {
    let written = entry();
    let stored = serde_json::to_value(&written).expect("the entry serializes");
    assert_eq!(stored["id"], json!("record-1:stdout"));
    assert_eq!(
        stored["command"],
        json!("cargo test"),
        "the command travels with the entry"
    );
    let read: ResultEntry = serde_json::from_value(stored).expect("the entry reads back");
    assert_eq!(read, written);
}

/// Absent fields stay absent rather than arriving as zero, so a reader can
/// tell a result whose exit was never observed from one that exited zero.
#[test]
fn test_unobserved_fields_stay_absent() {
    let mut written = entry();
    written.command = None;
    written.call = None;
    written.exit = None;
    written.created_ms = None;
    written.stop_reason = Some(CaptureStopReason::WallBudget);
    let stored = serde_json::to_value(&written).expect("the entry serializes");
    assert!(stored.get("command").is_none(), "no command is claimed");
    assert!(stored.get("call").is_none(), "no call is named");
    assert!(stored.get("exit").is_none(), "no status is claimed");
    assert_eq!(stored["stop_reason"], json!("wall_budget"));
    let read: ResultEntry = serde_json::from_value(stored).expect("the entry reads back");
    assert_eq!(read, written);
}

/// A read asks by byte or by line, and the two are told apart by the request
/// itself, so a host never has to guess which unit an offset is in.
#[test]
fn test_range_names_its_unit() {
    let by_bytes = serde_json::to_value(ResultRange::Bytes {
        offset: 100,
        len: 200,
    })
    .expect("the range serializes");
    assert_eq!(by_bytes, json!({"by": "bytes", "offset": 100, "len": 200}));
    let by_lines = serde_json::to_value(ResultRange::Lines {
        start: 5,
        count: 10,
    })
    .expect("the range serializes");
    assert_eq!(by_lines, json!({"by": "lines", "start": 5, "count": 10}));
    let read: ResultRange = serde_json::from_value(by_lines).expect("the range reads back");
    assert_eq!(
        read,
        ResultRange::Lines {
            start: 5,
            count: 10
        }
    );
}

/// A stored block names the field it replaced, and a whole-output block names
/// the empty field, so the reader gets the same address the tool result did.
#[test]
fn test_stored_stream_field() {
    let field = serde_json::to_value(ResultStream::Stored {
        field: "stdout".into(),
    })
    .expect("the stream serializes");
    assert_eq!(field, json!({"stream": "stored", "field": "stdout"}));
    let whole = serde_json::to_value(ResultStream::Stored {
        field: String::new(),
    })
    .expect("the stream serializes");
    assert_eq!(whole, json!({"stream": "stored", "field": ""}));
    assert_eq!(
        serde_json::to_value(ResultStream::Stderr).expect("the stream serializes"),
        json!({"stream": "stderr"})
    );
}

/// A window's end is where the next one begins, including at the end of the
/// addressable range, where the sum would overflow a narrower count.
#[test]
fn test_span_end_is_exclusive() {
    let span = ByteSpan { start: 10, len: 5 };
    assert_eq!(span.end(), 15);
    let wide = ByteSpan {
        start: u64::MAX - 1,
        len: 4,
    };
    assert_eq!(wide.end(), u64::MAX, "the end saturates rather than wraps");
}

/// The listing page and its cursor round-trip so a caller can hand back what
/// it received without rebuilding it.
#[test]
fn test_page_round_trips() {
    let page = ResultPage {
        entries: vec![entry()],
        next: Some(ResultCursor::new("v1|cursor")),
        complete: false,
    };
    let stored = serde_json::to_value(&page).expect("the page serializes");
    assert_eq!(stored["next"], json!("v1|cursor"));
    assert_eq!(stored["complete"], json!(false));
    let read: ResultPage = serde_json::from_value(stored).expect("the page reads back");
    assert_eq!(read, page);
}

/// A read that could not show its bytes as text says how many there are
/// instead of handing back a lossy decoding.
#[test]
fn test_opaque_content_counts_bytes() {
    let read = ResultRead {
        id: ResultId::new("record-1:stderr"),
        encoding: ResultEncoding::Raw,
        content: ResultContent::Opaque { bytes: 9 },
        covered: ByteSpan { start: 0, len: 9 },
        total: Some(9),
        completeness: ResultCompleteness::Truncated,
        next: None,
        line: None,
    };
    let stored = serde_json::to_value(&read).expect("the read serializes");
    assert_eq!(
        stored["content"],
        json!({"content": "opaque", "bytes": 9}),
        "the count is carried without the bytes"
    );
    let back: ResultRead = serde_json::from_value(stored).expect("the read reads back");
    assert_eq!(back, read);
}

/// A result name prints as the scalar it is, so a reader that formats one
/// into a message writes the same text it can hand back as a name.
#[test]
fn test_result_id_prints_name() {
    let id = ResultId::new("record-1:stderr");
    assert_eq!(id.to_string(), "record-1:stderr");
}

/// Raw bytes are the text a reader sees; the stored JSON form is not, which
/// is the distinction a reader needs before showing any of it.
#[test]
fn test_raw_marks_plain_bytes() {
    assert!(ResultEncoding::Raw.is_raw());
    assert!(!ResultEncoding::JsonStored.is_raw());
}
