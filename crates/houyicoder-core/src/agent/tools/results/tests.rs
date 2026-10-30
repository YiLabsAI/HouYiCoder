//! Tests for the stored-result catalog: what the fold finds in a log, how a
//! page continues, and how a window read behaves against a real in-memory
//! store.

use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use houyicoder_api::capture::{CallEnd, CapturedStream, CapturedStreams, ResultCaptureSource};
use houyicoder_api::result_catalog::{ResultCatalog, ResultCatalogError};
use houyicoder_api::session::SessionLog;
use houyicoder_async::PFut;
use houyicoder_context::{
    BlockHash, BlockWindow, CheckpointId, CheckpointManifest, ContextBackend, ContextError,
    EventId, SessionEvent, SessionId, SessionLogEntry,
};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::block_ref::BLOCK_REF_KEY;
use houyicoder_protocol::result_catalog::{
    ResultContent, ResultCursor, ResultFilter, ResultId, ResultListQuery, ResultPage, ResultRange,
    ResultReadQuery, ResultSearchQuery, ResultStream, ScanBudget,
};
use houyicoder_session::SessionStore;
use serde_json::{Map, Value, json};

use tokio::sync::Notify;

use super::SessionResultCatalog;
use super::entry::LineMarks;
use crate::agent::tools::result_capture::SessionResultCapture;

mod paging;

fn store() -> Arc<dyn SessionLog> {
    Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())))
}

fn entry(session: SessionId, event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 1,
        prev_hash: None,
        event,
    }
}

/// Log the boundary record a clear appends, which is what opens the new epoch.
fn clear(log: &Arc<dyn SessionLog>, session: SessionId) {
    let cleared = entry(session, SessionEvent::ContextCleared { prior_turn: 1 });
    pollster::block_on(log.append(cleared)).expect("the clear is logged");
}

/// Publish a capture of both streams and log the call and its bounded result,
/// which names the record the fold reads.
fn seed(log: &Arc<dyn SessionLog>, session: SessionId, stdout: &[u8], stderr: &[u8]) {
    seed_call(log, session, "call-1", "bash", stdout, stderr);
}

/// A listing reports one entry per published stream, named so a reader can
/// ask for it, with the tool that ran under that call.
#[tokio::test]
async fn test_list_reports_kept_streams() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"out", b"err");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = catalog
        .list(session, ResultListQuery::default())
        .await
        .expect("the listing answers");
    assert_eq!(page.entries.len(), 2, "one entry per stream");
    let streams: Vec<ResultStream> = page.entries.iter().map(|e| e.stream.clone()).collect();
    assert!(streams.contains(&ResultStream::Stdout), "{streams:?}");
    assert!(streams.contains(&ResultStream::Stderr), "{streams:?}");
    assert!(
        page.entries
            .iter()
            .all(|e| e.tool.as_deref() == Some("bash")),
        "the call names the tool"
    );
    assert!(
        page.entries
            .iter()
            .all(|e| e.command.as_deref() == Some("run")),
        "the row carries the command the call ran"
    );
    assert_eq!(page.entries[0].id.as_str(), "call-1:stdout");
}

/// A clear replaces the history the catalog answers for. An index folded
/// before it describes records the session no longer holds, so the next
/// listing must answer for the new epoch rather than reporting the old one as
/// complete.
#[tokio::test]
async fn test_clear_drops_earlier_results() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"before", b"before-err");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = catalog
        .list(session, ResultListQuery::default())
        .await
        .expect("the listing answers");
    assert_eq!(page.entries.len(), 2, "both streams of the earlier call");

    log.reset_trajectory(session);
    clear(&log, session);
    seed_call(&log, session, "call-2", "bash", b"after", b"after-err");

    let page = catalog
        .list(session, ResultListQuery::default())
        .await
        .expect("the listing answers");
    let ids: Vec<&str> = page.entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        ["call-2:stdout", "call-2:stderr"],
        "only the history after the clear is listed"
    );
}

/// A cursor names the epoch it was issued in, so one issued before a clear is
/// refused rather than followed into a history the session no longer holds.
#[tokio::test]
async fn test_clear_refuses_earlier_cursor() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"before", b"before-err");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = catalog
        .list(
            session,
            ResultListQuery {
                limit: Some(1),
                ..ResultListQuery::default()
            },
        )
        .await
        .expect("the listing answers");
    let stale = page.next.clone().expect("a second entry remains");

    log.reset_trajectory(session);
    clear(&log, session);
    seed_call(&log, session, "call-2", "bash", b"after", b"after-err");

    let error = catalog
        .list(
            session,
            ResultListQuery {
                limit: Some(1),
                cursor: Some(stale),
                ..ResultListQuery::default()
            },
        )
        .await
        .expect_err("a cursor from before the clear is refused");
    assert!(
        matches!(error, ResultCatalogError::InvalidCursor(_)),
        "{error}"
    );
}

/// A read returns the window it covered and a cursor; asking again with the
/// cursor continues where the first read stopped instead of repeating it.
#[tokio::test]
async fn test_read_continues_with_cursor() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"0123456789abcdefghijklmnopqrstuvwxyz", b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let first = catalog
        .read(
            session,
            ResultReadQuery {
                id: ResultId::new("call-1:stdout"),
                range: ResultRange::Bytes { offset: 0, len: 10 },
                cursor: None,
            },
        )
        .await
        .expect("the read answers");
    assert_eq!(first.covered.start, 0);
    assert_eq!(first.covered.len, 10);
    let next = first.next.clone().expect("more bytes remain");
    let second = catalog
        .read(
            session,
            ResultReadQuery {
                id: ResultId::new("call-1:stdout"),
                range: ResultRange::Bytes { offset: 0, len: 10 },
                cursor: Some(next),
            },
        )
        .await
        .expect("the continuation answers");
    assert_eq!(second.covered.start, 10, "the cursor moves the window");
}

/// A read that begins inside a character covers that character whole. The
/// span it reports names where its text starts, and the text is not a run of
/// bytes the reader has to reassemble around the offset it happened to name.
#[tokio::test]
async fn test_read_inside_character() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, "汉字 tail".as_bytes(), b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let read = catalog
        .read(
            session,
            ResultReadQuery {
                id: ResultId::new("call-1:stdout"),
                range: ResultRange::Bytes { offset: 1, len: 8 },
                cursor: None,
            },
        )
        .await
        .expect("the read answers");
    assert_eq!(read.covered.start, 0, "the span begins at the character");
    match read.content {
        ResultContent::Text { text } => {
            assert!(
                text.starts_with("汉字"),
                "the characters the offset landed in are decoded, not dropped: {text}"
            );
        }
        other => panic!("text expected: {other:?}"),
    }
}

/// A match that begins in one window and ends in the next is reported once,
/// at its stored offset, rather than lost at the boundary or counted twice.
#[tokio::test]
async fn test_search_finds_across_windows() {
    let log = store();
    let session = SessionId::new();
    let mut stdout = vec![b'x'; 130 * 1024];
    let at = 64 * 1024 - 3;
    stdout[at..at + 6].copy_from_slice(b"NEEDLE");
    seed(&log, session, &stdout, b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = catalog
        .search(
            session,
            ResultSearchQuery {
                query: "needle".to_string(),
                within: Some(ResultId::new("call-1:stdout")),
                cursor: None,
                budget: None,
            },
        )
        .await
        .expect("the search answers");
    assert_eq!(page.hits.len(), 1, "found once: {:?}", page.hits);
    assert_eq!(page.hits[0].span.start, at as u64);
    assert!(
        page.hits[0].snippet.contains("NEEDLE"),
        "{:?}",
        page.hits[0]
    );
    assert!(!page.partial, "the scan covered the stream");
}

/// The aggregate counts one result per event and one stream per published
/// side, and adds the kept bytes of each side separately.
#[tokio::test]
async fn test_stats_counts_streams() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, &[b'o'; 100], &[b'e'; 40]);
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let stats = catalog.stats(session).await.expect("the aggregate answers");
    assert_eq!(stats.results, 1, "both streams answer one call");
    assert_eq!(stats.streams, 2);
    assert_eq!(stats.complete_streams, 2);
    assert_eq!(stats.stdout_bytes, 100);
    assert_eq!(stats.stderr_bytes, 40);
    assert!(stats.complete, "the fold reached the end of the log");
    assert_eq!(stats.corrupt, 0);
}

/// A name the session never issued is not found, rather than answered with an
/// empty window a caller could read as content that is gone.
#[tokio::test]
async fn test_unknown_id_not_found() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"out", b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let error = catalog
        .read(
            session,
            ResultReadQuery {
                id: ResultId::new("call-9:stdout"),
                range: ResultRange::Bytes { offset: 0, len: 8 },
                cursor: None,
            },
        )
        .await
        .expect_err("an unknown name is not a result");
    assert!(matches!(error, ResultCatalogError::NotFound), "{error:?}");
}

/// A byte read starting past the end of a stream is refused with the length
/// the stream holds, rather than answered with nothing.
#[tokio::test]
async fn test_byte_past_end_refused() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"abc", b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let error = catalog
        .read(
            session,
            ResultReadQuery {
                id: ResultId::new("call-1:stdout"),
                range: ResultRange::Bytes { offset: 99, len: 4 },
                cursor: None,
            },
        )
        .await
        .expect_err("a byte past the end is not a range");
    assert!(
        matches!(error, ResultCatalogError::InvalidRange(_)),
        "{error:?}"
    );
}

/// Run one result request and hand back the answer, failing the test on a
/// refusal.
async fn list_of(
    catalog: &SessionResultCatalog,
    session: SessionId,
    filter: ResultFilter,
    cursor: Option<ResultCursor>,
    limit: Option<u32>,
) -> ResultPage {
    catalog
        .list(
            session,
            ResultListQuery {
                filter,
                cursor,
                limit,
            },
        )
        .await
        .expect("the listing answers")
}

/// Store bytes directly, as an earlier externalize step would have, and answer
/// the address a result names them by.
fn put_block(log: &Arc<dyn SessionLog>, bytes: &[u8]) -> String {
    let hash =
        pollster::block_on(log.backend().block_put(bytes.to_vec())).expect("a block is stored");
    hash.0
}

/// Log a call and the result that answers it, with the kept bytes published by
/// a real capture.
fn seed_call(
    log: &Arc<dyn SessionLog>,
    session: SessionId,
    call_id: &str,
    tool: &str,
    stdout: &[u8],
    stderr: &[u8],
) {
    let source = SessionResultCapture::new(Arc::clone(log));
    let capture = source
        .begin(session, call_id, tool)
        .expect("a capture source starts a capture");
    capture.receive(CapturedStream::Stdout, stdout);
    capture.receive(CapturedStream::Stderr, stderr);
    let published: CapturedStreams = pollster::block_on(capture.finish(CallEnd::Exited(Some(0))))
        .expect("finish succeeds")
        .expect("output is published");
    let call = entry(
        session,
        SessionEvent::ToolCall {
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            input: json!({"command": "run"}),
        },
    );
    pollster::block_on(log.append(call)).expect("the call is logged");
    let result = entry(
        session,
        SessionEvent::ToolResult {
            call_id: call_id.to_string(),
            output: json!({"raw_capture": {"record": {"block_ref": published.record.0}}}),
            duration_ms: 3,
        },
    );
    pollster::block_on(log.append(result)).expect("the result is logged");
}

/// Log a result whose output names a stored block under one field, or as the
/// whole output when no field is named.
fn seed_marker(
    log: &Arc<dyn SessionLog>,
    session: SessionId,
    call_id: &str,
    tool: &str,
    field: Option<&str>,
    hash: &str,
) {
    let call = entry(
        session,
        SessionEvent::ToolCall {
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            input: json!({}),
        },
    );
    pollster::block_on(log.append(call)).expect("the call is logged");
    let mut marker = Map::new();
    marker.insert(BLOCK_REF_KEY.to_string(), json!(hash));
    let mut output = Map::new();
    match field {
        Some(field) => output.insert(field.to_string(), Value::Object(marker)),
        None => output.insert(BLOCK_REF_KEY.to_string(), json!(hash)),
    };
    let result = entry(
        session,
        SessionEvent::ToolResult {
            call_id: call_id.to_string(),
            output: Value::Object(output),
            duration_ms: 3,
        },
    );
    pollster::block_on(log.append(result)).expect("the result is logged");
}

/// Log a result naming an address the store never held.
fn seed_missing(log: &Arc<dyn SessionLog>, session: SessionId, call_id: &str) {
    seed_marker(
        log,
        session,
        call_id,
        "bash",
        Some("captured"),
        &"0".repeat(64),
    );
}

/// A line read answers the lines asked for and says which line it began at,
/// so a reader can address the next range without counting bytes.
#[tokio::test]
async fn test_read_lines_window() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"alpha\nbeta\ngamma\ndelta\n", b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let read = catalog
        .read(
            session,
            ResultReadQuery {
                id: ResultId::new("call-1:stdout"),
                range: ResultRange::Lines { start: 2, count: 2 },
                cursor: None,
            },
        )
        .await
        .expect("the line read answers");
    match read.content {
        ResultContent::Text { text } => assert_eq!(text, "beta\ngamma\n", "two whole lines"),
        other => panic!("a line read is text: {other:?}"),
    }
    assert_eq!(read.line, Some(2), "the answer names its first line");
    assert_eq!(read.covered.start, 6, "the range starts at that line");
    assert_eq!(read.covered.len, 11);
    assert_eq!(read.total, Some(23));
    assert!(read.next.is_some(), "more lines remain past the range");
}

/// Line zero names nothing, because lines count from one, and is refused
/// rather than read as the first line.
#[tokio::test]
async fn test_line_zero_refused() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"only\n", b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let error = catalog
        .read(
            session,
            ResultReadQuery {
                id: ResultId::new("call-1:stdout"),
                range: ResultRange::Lines { start: 0, count: 1 },
                cursor: None,
            },
        )
        .await
        .expect_err("line zero is not a line");
    assert!(
        matches!(error, ResultCatalogError::InvalidRange(_)),
        "{error:?}"
    );
}

/// A line past the end of a result is refused with the size it does hold,
/// rather than answered with an empty range a reader could take for content.
#[tokio::test]
async fn test_line_read_past_end() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"only\n", b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let error = catalog
        .read(
            session,
            ResultReadQuery {
                id: ResultId::new("call-1:stdout"),
                range: ResultRange::Lines { start: 9, count: 1 },
                cursor: None,
            },
        )
        .await
        .expect_err("a line past the end is not a range");
    assert!(
        matches!(error, ResultCatalogError::InvalidRange(_)),
        "{error:?}"
    );
}

/// A line longer than one page is paged like any other text: the answer stops
/// at the byte bound, still names the line it stopped in, and the cursor
/// continues inside that line rather than skipping it.
#[tokio::test]
async fn test_line_pages_long_output() {
    let log = store();
    let session = SessionId::new();
    let body = "x".repeat(20_000);
    seed(&log, session, body.as_bytes(), b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let mut text = String::new();
    let mut cursor = None;
    loop {
        let read = catalog
            .read(
                session,
                ResultReadQuery {
                    id: ResultId::new("call-1:stdout"),
                    range: ResultRange::Lines { start: 1, count: 1 },
                    cursor: cursor.clone(),
                },
            )
            .await
            .expect("the page answers");
        match read.content {
            ResultContent::Text { text: page } => {
                assert!(
                    page.len() <= 8_000,
                    "a page stays within its bound: {}",
                    page.len()
                );
                text.push_str(&page);
            }
            other => panic!("a long line is text: {other:?}"),
        }
        assert_eq!(read.line, Some(1), "the pages stay on the one line");
        match read.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(text, body, "every byte of the line is read once");
}

/// A line between two marks resumes from the stride mark before it. Starting
/// from the block would rescan more than the line asks for, and starting from
/// the mark after it would skip the line wanted.
#[tokio::test]
async fn test_line_resumes_from_mark() {
    let log = store();
    let session = SessionId::new();
    let body: String = (1..=1200).map(|n| format!("l{n}\n")).collect();
    seed(&log, session, body.as_bytes(), b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let read_line = |start: u64| {
        let catalog = &catalog;
        async move {
            catalog
                .read(
                    session,
                    ResultReadQuery {
                        id: ResultId::new("call-1:stdout"),
                        range: ResultRange::Lines { start, count: 1 },
                        cursor: None,
                    },
                )
                .await
                .expect("the line read answers")
        }
    };
    read_line(1024).await;
    let earlier = read_line(500).await;
    match earlier.content {
        ResultContent::Text { text } => assert_eq!(text, "l500\n", "the line asked for"),
        other => panic!("a line read is text: {other:?}"),
    }
    assert_eq!(earlier.line, Some(500), "the answer names its line");
}

/// A mark answers the stride line it covers, whichever stride it is, and the
/// call that would index one stride off answers nothing instead of the
/// neighbouring position. Reading a stride line after later marks exist is
/// what the marks exist for, so a wrong index there costs a full rescan from
/// the block start.
#[test]
fn test_marks_answer_stride_lines() {
    let mut marks = LineMarks::default();
    marks.record(256, 100);
    marks.record(512, 200);
    marks.record(768, 300);
    marks.record(600, 250);
    assert_eq!(marks.get(256), Some(100), "the first stride");
    assert_eq!(marks.get(512), Some(200), "a middle stride");
    assert_eq!(marks.get(768), Some(300), "the last stride");
    assert_eq!(marks.get(500), None, "a line between strides has no mark");
    assert_eq!(marks.get(0), None, "line zero is not a stride");
    assert_eq!(
        marks.at_or_before(768),
        Some((768, 300)),
        "the furthest mark"
    );
}

/// A line read of an earlier stride after a far read still answers that line,
/// which is the answer a reader depends on whether or not a mark covered it.
#[tokio::test]
async fn test_line_read_after_far() {
    let log = store();
    let session = SessionId::new();
    let body: String = (1..=600).map(|n| format!("l{n}\n")).collect();
    seed(&log, session, body.as_bytes(), b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let read_line = |start: u64| {
        let catalog = &catalog;
        async move {
            catalog
                .read(
                    session,
                    ResultReadQuery {
                        id: ResultId::new("call-1:stdout"),
                        range: ResultRange::Lines { start, count: 1 },
                        cursor: None,
                    },
                )
                .await
                .expect("the line read answers")
        }
    };
    read_line(600).await;
    let earlier = read_line(512).await;
    match earlier.content {
        ResultContent::Text { text } => assert_eq!(text, "l512\n", "the earlier line is found"),
        other => panic!("a line read is text: {other:?}"),
    }
    assert_eq!(earlier.line, Some(512), "the answer names its line");
}

/// A listing filter restricts the rows by tool and by kept size, so a reader
/// looking for one tool's output does not page through every result.
#[tokio::test]
async fn test_list_filters_by_tool() {
    let log = store();
    let session = SessionId::new();
    seed_call(&log, session, "call-1", "bash", &[b'o'; 100], b"");
    seed_call(&log, session, "call-2", "edit", b"tiny", b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let only = |tool: Option<&str>, min: Option<u64>| ResultFilter {
        tool: tool.map(str::to_string),
        since_ms: None,
        until_ms: None,
        min_bytes: min,
    };
    let edits = list_of(&catalog, session, only(Some("edit"), None), None, None).await;
    assert_eq!(
        edits.entries.len(),
        1,
        "one tool's rows: {:?}",
        edits.entries
    );
    assert_eq!(edits.entries[0].id.as_str(), "call-2:stdout");
    let big = list_of(&catalog, session, only(None, Some(50)), None, None).await;
    assert_eq!(
        big.entries.len(),
        1,
        "one row is big enough: {:?}",
        big.entries
    );
    assert_eq!(big.entries[0].id.as_str(), "call-1:stdout");
    let early = ResultFilter {
        since_ms: Some(u64::MAX),
        ..only(None, None)
    };
    let none = list_of(&catalog, session, early, None, None).await;
    assert!(none.entries.is_empty(), "no call began that late");
}

/// A page holds the size asked for and hands back a cursor; asking again from
/// that cursor returns the rows after it rather than repeating the page.
#[tokio::test]
async fn test_list_pages_with_cursor() {
    let log = store();
    let session = SessionId::new();
    for call in ["call-1", "call-2", "call-3"] {
        seed_call(&log, session, call, "bash", b"out", b"");
    }
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let first = list_of(&catalog, session, ResultFilter::default(), None, Some(1)).await;
    assert_eq!(first.entries.len(), 1, "a page of one");
    let next = first.next.clone().expect("more rows remain");
    let second = list_of(
        &catalog,
        session,
        ResultFilter::default(),
        Some(next),
        Some(1),
    )
    .await;
    assert_eq!(second.entries.len(), 1);
    assert_ne!(
        first.entries[0].id, second.entries[0].id,
        "the page continues rather than repeating"
    );
}

/// A cursor this host did not issue for this session and this request is
/// refused with the reason, so a caller retries instead of reading whatever
/// position the text happens to name.
#[tokio::test]
async fn test_cursors_refused_outside_scope() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"out", b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let other = SessionId::new();
    let cursors = [
        "v2|nonsense".to_string(),
        format!("v1|{other}|list|0|0|0|0"),
        format!("v1|{session}|read|0|0|0|0"),
        format!("v1|{session}|list|0|0|0|0|0"),
        format!("v1|{session}|bogus|0|0|0|0"),
        "lonely".to_string(),
        format!("v1|{session}|list|0|x|0|0"),
        format!("v1|{session}|list|0|0|99|0"),
    ];
    for cursor in cursors {
        let error = catalog
            .list(
                session,
                ResultListQuery {
                    cursor: Some(ResultCursor::new(cursor.clone())),
                    ..ResultListQuery::default()
                },
            )
            .await
            .expect_err("a foreign cursor is refused");
        assert!(
            matches!(error, ResultCatalogError::InvalidCursor(_)),
            "cursor {cursor:?}: {error:?}"
        );
    }
}

/// A cursor issued for one listing is refused by a listing whose filter
/// answers with different rows, because the position it names means nothing
/// in the other request.
#[tokio::test]
async fn test_cursor_bound_to_request() {
    let log = store();
    let session = SessionId::new();
    for call in ["call-1", "call-2"] {
        seed_call(&log, session, call, "bash", b"out", b"");
    }
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let first = list_of(&catalog, session, ResultFilter::default(), None, Some(1)).await;
    let next = first.next.clone().expect("more rows remain");
    let other = ResultFilter {
        tool: Some("edit".to_string()),
        ..ResultFilter::default()
    };
    let error = catalog
        .list(
            session,
            ResultListQuery {
                filter: other,
                cursor: Some(next),
                limit: Some(1),
            },
        )
        .await
        .expect_err("a cursor from another request is refused");
    assert!(
        matches!(error, ResultCatalogError::InvalidCursor(_)),
        "{error:?}"
    );
}

/// A cursor whose position was edited after the host issued it is refused
/// rather than followed, because the check value no longer covers the text.
#[tokio::test]
async fn test_cursor_tamper_refused() {
    let log = store();
    let session = SessionId::new();
    for call in ["call-1", "call-2"] {
        seed_call(&log, session, call, "bash", b"out", b"");
    }
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let first = list_of(&catalog, session, ResultFilter::default(), None, Some(1)).await;
    let issued = first.next.clone().expect("more rows remain");
    let mut parts: Vec<String> = issued.as_str().split('|').map(str::to_string).collect();
    let offset: u64 = parts[6].parse().expect("the cursor carries an offset");
    parts[6] = (offset + 1).to_string();
    let tampered = ResultCursor::new(parts.join("|"));
    let error = catalog
        .list(
            session,
            ResultListQuery {
                cursor: Some(tampered),
                ..ResultListQuery::default()
            },
        )
        .await
        .expect_err("an edited cursor is refused");
    assert!(
        matches!(error, ResultCatalogError::InvalidCursor(_)),
        "{error:?}"
    );
}

/// A search that runs out of budget answers with what it found and a cursor,
/// and continuing from that cursor finds the match that began before the stop.
#[tokio::test]
async fn test_search_continues_past_budget() {
    let log = store();
    let session = SessionId::new();
    let mut stdout = vec![b'x'; 130 * 1024];
    let at = 64 * 1024 - 3;
    stdout[at..at + 6].copy_from_slice(b"NEEDLE");
    seed(&log, session, &stdout, b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let query = |cursor: Option<ResultCursor>| ResultSearchQuery {
        query: "needle".to_string(),
        within: Some(ResultId::new("call-1:stdout")),
        cursor,
        budget: Some(ScanBudget {
            max_bytes: 1,
            max_millis: 5_000,
        }),
    };
    let first = catalog
        .search(session, query(None))
        .await
        .expect("the search answers");
    assert!(first.partial, "the scan stopped at its budget");
    let next = first.next.clone().expect("a stopped scan continues");
    let rest = catalog
        .search(session, query(Some(next)))
        .await
        .expect("the continuation answers");
    assert_eq!(
        rest.hits.len(),
        1,
        "the split match is found: {:?}",
        rest.hits
    );
    assert_eq!(rest.hits[0].span.start, at as u64);
}

/// A scan whose budget runs out on its first window still moves its cursor, so
/// a caller that keeps asking reaches the end instead of asking for the same
/// window forever.
#[tokio::test]
async fn test_search_small_budget_advances() {
    let log = store();
    let session = SessionId::new();
    let mut stdout = vec![b'x'; 130 * 1024];
    stdout[..6].copy_from_slice(b"needle");
    seed(&log, session, &stdout, b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let query = |cursor: Option<ResultCursor>| ResultSearchQuery {
        query: "needle".to_string(),
        within: Some(ResultId::new("call-1:stdout")),
        cursor,
        budget: Some(ScanBudget {
            // A zero budget is raised to one byte on the way in, so this is
            // the smallest scan a caller can ask for.
            max_bytes: 0,
            max_millis: 0,
        }),
    };
    let mut cursor = None;
    let mut cursors: Vec<String> = Vec::new();
    for _ in 0..8 {
        let page = catalog
            .search(session, query(cursor.clone()))
            .await
            .expect("the page answers");
        let Some(next) = page.next else {
            return;
        };
        let text = next.as_str().to_string();
        assert!(
            !cursors.contains(&text),
            "a page never repeats a cursor: {text}"
        );
        cursors.push(text);
        cursor = Some(next);
    }
    panic!("the paging reaches the end of the stream");
}

/// A hit found by a scan that began at the stream's start names the line it
/// sits on, counting from one, so a reader can jump straight to it.
#[tokio::test]
async fn test_search_names_hit_line() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"one\ntwo\nthree needle\nfour\n", b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = catalog
        .search(
            session,
            ResultSearchQuery {
                query: "needle".to_string(),
                within: Some(ResultId::new("call-1:stdout")),
                cursor: None,
                budget: None,
            },
        )
        .await
        .expect("the search answers");
    assert_eq!(page.hits.len(), 1, "{:?}", page.hits);
    assert_eq!(page.hits[0].line, Some(3), "the third line holds the match");
}

/// Bytes that are not text are skipped, and the positions after the gap stay
/// mapped to their stored offsets, so a hit past one names where its bytes
/// really are.
#[tokio::test]
async fn test_search_after_undecodable_byte() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"ab\xFFcd", b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = catalog
        .search(
            session,
            ResultSearchQuery {
                query: "cd".to_string(),
                within: Some(ResultId::new("call-1:stdout")),
                cursor: None,
                budget: None,
            },
        )
        .await
        .expect("the search answers");
    assert_eq!(page.hits.len(), 1, "{:?}", page.hits);
    assert_eq!(
        page.hits[0].span.start, 3,
        "the match begins after the byte the reader could not decode"
    );
}

/// A match that ends before a window boundary is reported by that window and
/// not again by the next one, which reads the tail before its start.
#[tokio::test]
async fn test_search_reports_hit_once() {
    let log = store();
    let session = SessionId::new();
    let mut stdout = vec![b'x'; 130 * 1024];
    stdout[64 * 1024 - 20..64 * 1024 - 14].copy_from_slice(b"NEEDLE");
    let at = 64 * 1024 - 3;
    stdout[at..at + 6].copy_from_slice(b"NEEDLE");
    seed(&log, session, &stdout, b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = catalog
        .search(
            session,
            ResultSearchQuery {
                query: "needle".to_string(),
                within: Some(ResultId::new("call-1:stdout")),
                cursor: None,
                budget: None,
            },
        )
        .await
        .expect("the search answers");
    assert_eq!(page.hits.len(), 2, "each match once: {:?}", page.hits);
    assert_eq!(page.hits[0].span.start, (64 * 1024 - 20) as u64);
    assert_eq!(page.hits[1].span.start, at as u64);
}

/// A page holds no more hits than its ceiling, and the cursor continues at the
/// match after the last one rather than past the rest of the window.
#[tokio::test]
async fn test_search_page_respects_limit() {
    let log = store();
    let session = SessionId::new();
    let mut stdout = String::new();
    for _ in 0..12 {
        stdout.push_str("needle ");
    }
    seed(&log, session, stdout.as_bytes(), b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let query = |cursor: Option<ResultCursor>| ResultSearchQuery {
        query: "needle".to_string(),
        within: Some(ResultId::new("call-1:stdout")),
        cursor,
        budget: None,
    };
    let first = catalog
        .search(session, query(None))
        .await
        .expect("the search answers");
    assert_eq!(first.hits.len(), 10, "the page stops at its ceiling");
    assert!(first.partial, "more matches remain");
    let next = first.next.clone().expect("a full page continues");
    let rest = catalog
        .search(session, query(Some(next)))
        .await
        .expect("the continuation answers");
    assert_eq!(rest.hits.len(), 2, "the rest of the window is read");
    assert!(!rest.partial, "nothing follows the last match");
}

/// A search over a block stored as a JSON string pages without repeating a
/// hit. Every page walks the escapes from the beginning of the block, so the
/// position a page hands back has to move past what the earlier page reported
/// rather than back to where the walk started.
#[tokio::test]
async fn test_search_pages_escaped() {
    let log = store();
    let session = SessionId::new();
    let body = format!(
        "{}needle{}needle{}",
        "x".repeat(200_000),
        "y".repeat(200_000),
        "z".repeat(1_000)
    );
    let literal = format!("\"{body}\"");
    let hash = put_block(&log, literal.as_bytes());
    seed_marker(&log, session, "call-1", "bash", Some("captured"), &hash);
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = list_of(&catalog, session, ResultFilter::default(), None, None).await;
    let id = page.entries[0].id.clone();
    let query = |cursor: Option<ResultCursor>| ResultSearchQuery {
        query: "needle".to_string(),
        within: Some(id.clone()),
        cursor,
        budget: Some(ScanBudget {
            max_bytes: 1_000,
            max_millis: 5_000,
        }),
    };
    let mut cursor = None;
    let mut spans = Vec::new();
    let mut cursors: Vec<String> = Vec::new();
    let mut finished = false;
    for _ in 0..32 {
        let page = catalog
            .search(session, query(cursor.clone()))
            .await
            .expect("the page answers");
        spans.extend(page.hits.iter().map(|hit| hit.span.start));
        match page.next {
            Some(next) => {
                let text = next.as_str().to_string();
                assert!(
                    !cursors.contains(&text),
                    "a page never hands back a cursor it already issued: {text}"
                );
                cursors.push(text);
                cursor = Some(next);
            }
            None => {
                finished = true;
                break;
            }
        }
    }
    assert!(finished, "the paging reaches the end of the block");
    assert_eq!(spans, [200_001, 400_007], "each hit once, in order");
}

/// A search scoped to a name the session never issued is refused, so a
/// mistyped name does not read as a search with no matches.
#[tokio::test]
async fn test_search_scoped_unknown_fails() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, b"out", b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let error = catalog
        .search(
            session,
            ResultSearchQuery {
                query: "out".to_string(),
                within: Some(ResultId::new("call-9:stdout")),
                cursor: None,
                budget: None,
            },
        )
        .await
        .expect_err("an unknown name is not a search scope");
    assert!(matches!(error, ResultCatalogError::NotFound), "{error:?}");
}

/// An output an earlier path externalized whole is listed as one result, and
/// reading it returns the stored bytes rather than a marker.
#[tokio::test]
async fn test_marker_reads_whole_output() {
    let log = store();
    let session = SessionId::new();
    let hash = put_block(&log, br#"{"stdout":"hi"}"#);
    seed_marker(&log, session, "call-1", "bash", None, &hash);
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = list_of(&catalog, session, ResultFilter::default(), None, None).await;
    assert_eq!(page.entries.len(), 1, "{:?}", page.entries);
    let row = &page.entries[0];
    assert_eq!(
        row.stream,
        ResultStream::Stored {
            field: String::new()
        }
    );
    assert_eq!(row.bytes, Some(15), "the stored length is reported");
    let read = catalog
        .read(
            session,
            ResultReadQuery {
                id: row.id.clone(),
                range: ResultRange::Bytes { offset: 0, len: 64 },
                cursor: None,
            },
        )
        .await
        .expect("the stored block is read");
    match read.content {
        ResultContent::Text { text } => assert_eq!(text, r#"{"stdout":"hi"}"#),
        other => panic!("the stored text is read back: {other:?}"),
    }
}

/// A field stored as a JSON string is read with its escapes undone, and its
/// byte count is left out because the escaped length is not the text's.
#[tokio::test]
async fn test_marker_json_string_decoded() {
    let log = store();
    let session = SessionId::new();
    let hash = put_block(&log, b"\"line one\\nline two \\u00e9\"");
    seed_marker(&log, session, "call-1", "bash", Some("captured"), &hash);
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = list_of(&catalog, session, ResultFilter::default(), None, None).await;
    assert_eq!(page.entries.len(), 1, "{:?}", page.entries);
    let row = &page.entries[0];
    assert_eq!(
        row.stream,
        ResultStream::Stored {
            field: "captured".into()
        }
    );
    assert_eq!(row.bytes, None, "the escaped length is not the text's");
    let read = catalog
        .read(
            session,
            ResultReadQuery {
                id: row.id.clone(),
                range: ResultRange::Bytes { offset: 0, len: 64 },
                cursor: None,
            },
        )
        .await
        .expect("the stored string is read");
    match read.content {
        ResultContent::Text { text } => assert_eq!(text, "line one\nline two é"),
        other => panic!("the decoded text is read back: {other:?}"),
    }
}

/// A JSON string read one byte at a time yields its text once, with every
/// window starting at the stored byte that holds the character it decodes:
/// an escape shifts the two positions apart, and a window after one that
/// reported the wrong start would repeat or skip a character.
#[tokio::test]
async fn test_json_string_after_escape() {
    let log = store();
    let session = SessionId::new();
    let hash = put_block(&log, b"\"ab\\ncd\\nef\"");
    seed_marker(&log, session, "call-1", "bash", Some("captured"), &hash);
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = list_of(&catalog, session, ResultFilter::default(), None, None).await;
    let id = page.entries[0].id.clone();
    let mut text = String::new();
    let mut starts = Vec::new();
    let mut cursor = None;
    loop {
        let read = catalog
            .read(
                session,
                ResultReadQuery {
                    id: id.clone(),
                    range: ResultRange::Bytes { offset: 0, len: 1 },
                    cursor: cursor.clone(),
                },
            )
            .await
            .expect("each window is read");
        match read.content {
            ResultContent::Text { text: window } => {
                starts.push(read.covered.start);
                text.push_str(&window);
            }
            other => panic!("a decoded string is text: {other:?}"),
        }
        match read.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(text, "ab\ncd\nef", "every character once, in order");
    assert_eq!(
        starts,
        vec![1, 2, 3, 5, 6, 7, 9, 10, 12],
        "each window starts at its character's stored byte, and the read at the closing quote covers nothing"
    );
}

/// A window that holds bytes no character decodes from is reported as bytes
/// at that span rather than as empty text, so a reader is told there is
/// content it cannot see and where it is.
#[tokio::test]
async fn test_binary_read_is_opaque() {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, &[0xFF, 0xFE, 0xFF, 0xFE], b"");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let read = catalog
        .read(
            session,
            ResultReadQuery {
                id: ResultId::new("call-1:stdout"),
                range: ResultRange::Bytes { offset: 0, len: 8 },
                cursor: None,
            },
        )
        .await
        .expect("the window is read");
    match read.content {
        ResultContent::Opaque { bytes } => assert_eq!(bytes, 4, "the span is reported"),
        other => panic!("undecodable bytes are opaque: {other:?}"),
    }
    assert_eq!(read.covered.start, 0);
    assert!(read.next.is_none(), "the window reached the end");
}

/// A capture record naming another session is not folded into this one, so a
/// result cannot answer this scope with bytes it does not own.
#[tokio::test]
async fn test_foreign_capture_record_skipped() {
    let log = store();
    let session = SessionId::new();
    let other = SessionId::new();
    let source = SessionResultCapture::new(Arc::clone(&log));
    let capture = source
        .begin(other, "call-1", "bash")
        .expect("a capture source starts a capture");
    capture.receive(CapturedStream::Stdout, b"theirs");
    let published: CapturedStreams = pollster::block_on(capture.finish(CallEnd::Exited(Some(0))))
        .expect("finish succeeds")
        .expect("output is published");
    let result = entry(
        session,
        SessionEvent::ToolResult {
            call_id: "call-1".to_string(),
            output: json!({"raw_capture": {"record": {"block_ref": published.record.0}}}),
            duration_ms: 3,
        },
    );
    pollster::block_on(log.append(result)).expect("the result is logged");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = list_of(&catalog, session, ResultFilter::default(), None, None).await;
    assert!(
        page.entries.is_empty(),
        "their bytes are not listed: {:?}",
        page.entries
    );
    let stats = catalog.stats(session).await.expect("the aggregate answers");
    assert_eq!(stats.corrupt, 1, "the record is reported unreadable");
    assert!(
        !stats.complete,
        "an unreadable record leaves the fold partial"
    );
}

/// A result naming a block the store does not hold is counted unreadable and
/// not listed, so a gap is reported rather than read as a result never kept.
#[tokio::test]
async fn test_missing_block_counts_corrupt() {
    let log = store();
    let session = SessionId::new();
    seed_missing(&log, session, "call-1");
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let page = list_of(&catalog, session, ResultFilter::default(), None, None).await;
    assert!(page.entries.is_empty(), "{:?}", page.entries);
    let stats = catalog.stats(session).await.expect("the aggregate answers");
    assert_eq!(stats.corrupt, 1);
    assert_eq!(stats.skipped_bytes, 0);
    assert!(
        !stats.complete,
        "an unreadable block leaves the figure a floor"
    );
}

/// A store whose windowed block reads can be held open partway through a
/// page, so a test can drop a request the way a vanished client does.
#[derive(Clone)]
struct HeldReads {
    state: Arc<HoldState>,
}

struct HoldState {
    inner: InMemoryBackend,
    holding: AtomicBool,
    reads: AtomicUsize,
    arrived: Notify,
}

impl HeldReads {
    fn new() -> Self {
        Self {
            state: Arc::new(HoldState {
                inner: InMemoryBackend::new(),
                holding: AtomicBool::new(false),
                reads: AtomicUsize::new(0),
                arrived: Notify::new(),
            }),
        }
    }

    /// Hold the second block read from now on. The first one completes, so
    /// the page has folded one record before the request is dropped.
    fn hold_second_read(&self) {
        self.state.reads.store(0, Ordering::SeqCst);
        self.state.holding.store(true, Ordering::SeqCst);
    }

    /// Let every later read through.
    fn open(&self) {
        self.state.holding.store(false, Ordering::SeqCst);
    }

    async fn wait_until_held(&self) {
        self.state.arrived.notified().await;
    }
}

impl ContextBackend for HeldReads {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        self.state.inner.append(event)
    }

    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.state.inner.read_range(session, from, to)
    }

    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.state.inner.replay(session)
    }

    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        self.state.inner.write_checkpoint(manifest)
    }

    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        self.state.inner.read_checkpoint(id)
    }

    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        self.state.inner.list_checkpoints(session)
    }

    fn block_put(&self, block: Vec<u8>) -> PFut<'_, Result<BlockHash, ContextError>> {
        self.state.inner.block_put(block)
    }

    fn block_get_range(
        &self,
        hash: &BlockHash,
        offset: u64,
        len: usize,
    ) -> PFut<'_, Result<BlockWindow, ContextError>> {
        let hold = self.state.holding.load(Ordering::SeqCst)
            && self.state.reads.fetch_add(1, Ordering::SeqCst) == 1;
        let hash = hash.clone();
        Box::pin(async move {
            if hold {
                self.state.arrived.notify_one();
                // The read never answers: the request waiting on it is
                // dropped instead, which is what a vanished client does.
                pending::<()>().await;
            }
            self.state.inner.block_get_range(&hash, offset, len).await
        })
    }
}

/// A request dropped in the middle of a page leaves the index exactly as it
/// was, so the retry folds the whole page and lists every result once. A fold
/// that kept what it had read so far would list the first record's stream
/// twice here.
#[tokio::test]
async fn test_dropped_fold_repeats_page() {
    let held = HeldReads::new();
    let log: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(held.clone())));
    let session = SessionId::new();
    seed_call(&log, session, "call-1", "bash", b"first", b"");
    seed_call(&log, session, "call-2", "bash", b"second", b"");
    let catalog = Arc::new(SessionResultCatalog::new(Arc::clone(&log)));
    held.hold_second_read();
    let dropped = {
        let catalog = Arc::clone(&catalog);
        tokio::spawn(async move { catalog.list(session, ResultListQuery::default()).await })
    };
    held.wait_until_held().await;
    dropped.abort();
    assert!(dropped.await.is_err(), "the request was dropped");
    held.open();
    let page = catalog
        .list(session, ResultListQuery::default())
        .await
        .expect("the retry answers");
    assert_eq!(page.entries.len(), 2, "{:?}", page.entries);
    let mut names: Vec<&str> = page.entries.iter().map(|e| e.id.as_str()).collect();
    let listed = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), listed, "no result is listed twice: {names:?}");
}
