//! Tests for the answers the recall tool gives about stored results.

use std::sync::Arc;

use houyicoder_api::capture::{CallEnd, CapturedStream, CapturedStreams, ResultCaptureSource};
use houyicoder_api::session::SessionLog;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::block_ref::BLOCK_REF_KEY;
use houyicoder_protocol::extension::ToolError;
use houyicoder_session::SessionStore;
use serde_json::{Map, Value, json};

use super::{Request, serve};
use crate::agent::tools::result_capture::SessionResultCapture;
use crate::agent::tools::results::SessionResultCatalog;

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

/// Publish one call's output through a real capture and log the call and the
/// result that names it, which is what the fold reads.
fn seed(
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
            input: json!({}),
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

/// Store bytes directly and log the result that names them under one field, or
/// as the whole output when no field is named.
fn seed_block(log: &Arc<dyn SessionLog>, session: SessionId, bytes: &[u8], field: Option<&str>) {
    let hash = pollster::block_on(log.backend().block_put(bytes.to_vec()))
        .expect("a block is stored")
        .0;
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
            call_id: "call-1".to_string(),
            output: Value::Object(output),
            duration_ms: 3,
        },
    );
    pollster::block_on(log.append(result)).expect("the result is logged");
}

/// Log a result whose field names a block that was never stored, so the fold
/// meets a record it cannot read.
fn seed_missing_block(log: &Arc<dyn SessionLog>, session: SessionId) {
    let mut marker = Map::new();
    marker.insert(BLOCK_REF_KEY.to_string(), json!("0".repeat(64)));
    let mut output = Map::new();
    output.insert("captured".to_string(), Value::Object(marker));
    let result = entry(
        session,
        SessionEvent::ToolResult {
            call_id: "call-1".to_string(),
            output: Value::Object(output),
            duration_ms: 3,
        },
    );
    pollster::block_on(log.append(result)).expect("the result is logged");
}

/// Answer one request the way the tool does, from the request text.
async fn ask(
    catalog: &SessionResultCatalog,
    session: SessionId,
    request: Value,
) -> Result<String, ToolError> {
    let request: Request = serde_json::from_value(request).expect("the request parses");
    serve(catalog, session, request).await
}

/// One captured call with both streams plus its log record, so the tests below
/// each start from a session that kept something.
fn seeded(stdout: &[u8], stderr: &[u8]) -> (Arc<dyn SessionLog>, SessionId) {
    let log = store();
    let session = SessionId::new();
    seed(&log, session, "call-1", "bash", stdout, stderr);
    (log, session)
}

/// A listing names each stream of the call that kept it, says what the call
/// was and how much it kept, and a filter that matches nothing says so rather
/// than answering with an empty list.
#[tokio::test]
async fn test_list_names_each_stream() {
    let (log, session) = seeded(b"out", b"err");
    let catalog = SessionResultCatalog::new(log);
    let all = ask(&catalog, session, json!({"op": "list"}))
        .await
        .expect("the listing answers");
    assert!(
        all.contains("call-1:stdout | bash | stdout | 3 B | complete, exit 0"),
        "{all}"
    );
    assert!(all.contains("call-1:stderr | bash | stderr | 3 B"), "{all}");
    let other = ask(&catalog, session, json!({"op": "list", "tool": "edit"}))
        .await
        .expect("the filtered listing answers");
    assert!(
        other.contains("Nothing this session kept matches that filter."),
        "{other}"
    );
}

/// A session that kept nothing says exactly that, so an empty listing is not
/// confused with a filter that matched nothing.
#[tokio::test]
async fn test_list_reports_nothing_kept() {
    let log = store();
    let session = SessionId::new();
    let catalog = SessionResultCatalog::new(log);
    let answer = ask(&catalog, session, json!({"op": "list"}))
        .await
        .expect("the listing answers");
    assert!(
        answer.contains("Nothing is kept beyond what the transcript shows."),
        "{answer}"
    );
}

/// A listing over a log the fold could not read whole answers for the part it
/// read, rather than claiming nothing was kept.
#[tokio::test]
async fn test_empty_list_names_unread() {
    let log = store();
    let session = SessionId::new();
    seed_missing_block(&log, session);
    let catalog = SessionResultCatalog::new(log);
    let answer = ask(&catalog, session, json!({"op": "list"}))
        .await
        .expect("the listing answers");
    assert!(
        answer.contains(
            "Nothing is kept in the part of the log this session could read; some of it was not read."
        ),
        "{answer}"
    );
}

/// A page that holds every readable row and still cannot answer for the whole
/// log says part of it was unread, rather than naming rows that do not exist.
#[tokio::test]
async fn test_list_page_reports_unread() {
    let log = store();
    let session = SessionId::new();
    seed_block(&log, session, b"kept", Some("captured"));
    seed_missing_block(&log, session);
    let catalog = SessionResultCatalog::new(log);
    let answer = ask(&catalog, session, json!({"op": "list"}))
        .await
        .expect("the listing answers");
    assert!(answer.contains("field captured"), "{answer}");
    assert!(
        answer.contains("Part of the log was not read, so results kept there are not listed."),
        "{answer}"
    );
    assert!(
        !answer.contains("more than this page covers"),
        "no row is left to page through: {answer}"
    );
}

/// A page says the log holds more and hands back the cursor that reaches it;
/// asking again from that cursor answers the rows after the page.
#[tokio::test]
async fn test_list_pages_with_cursor() {
    let log = store();
    let session = SessionId::new();
    for call in ["call-1", "call-2", "call-3"] {
        seed(&log, session, call, "bash", b"out", b"");
    }
    let catalog = SessionResultCatalog::new(log);
    let first = ask(&catalog, session, json!({"op": "list", "limit": 1}))
        .await
        .expect("the listing answers");
    assert!(first.contains("call-1:stdout"), "{first}");
    assert!(first.contains("Continue with cursor:"), "{first}");
    let cursor = first
        .lines()
        .find_map(|line| line.strip_prefix("Continue with cursor: "))
        .expect("the page names its cursor")
        .to_string();
    let second = ask(
        &catalog,
        session,
        json!({"op": "list", "limit": 1, "cursor": cursor}),
    )
    .await
    .expect("the continuation answers");
    assert!(second.contains("call-2:stdout"), "{second}");
    assert!(
        !second.contains("call-1:stdout"),
        "the page continues: {second}"
    );
}

/// A read answers the window it covered and where it sits in the whole stream,
/// and names the cursor that continues it.
#[tokio::test]
async fn test_read_serves_text() {
    let (log, session) = seeded(b"abcdefg", b"");
    let catalog = SessionResultCatalog::new(log);
    let answer = ask(
        &catalog,
        session,
        json!({"op": "read", "id": "call-1:stdout", "offset": 0, "len": 4}),
    )
    .await
    .expect("the read answers");
    assert!(answer.contains("bytes 0-4 of 7"), "{answer}");
    assert!(answer.contains("abcd"), "{answer}");
    assert!(answer.contains("Continues with cursor:"), "{answer}");
    let capped = ask(
        &catalog,
        session,
        json!({"op": "read", "id": "call-1:stdout", "len": 9000}),
    )
    .await
    .expect("the read answers");
    assert!(capped.contains("bytes 0-7 of 7"), "{capped}");
}

/// A read that names a line takes the line count it was given, so a caller
/// reading source output asks in the unit it was written in.
#[tokio::test]
async fn test_read_line_range_default() {
    let (log, session) = seeded(b"first\nsecond\nthird\n", b"");
    let catalog = SessionResultCatalog::new(log);
    let answer = ask(
        &catalog,
        session,
        json!({"op": "read", "id": "call-1:stdout", "line": 1}),
    )
    .await
    .expect("the line read answers");
    assert!(answer.contains("first\nsecond\nthird\n"), "{answer}");
}

/// A search inside a result reports each match with its offset and the words
/// around it, and a search that finds nothing says so.
#[tokio::test]
async fn test_search_reports_matches() {
    let (log, session) = seeded(b"a needle here", b"");
    let catalog = SessionResultCatalog::new(log);
    let found = ask(
        &catalog,
        session,
        json!({"op": "search", "query": "needle", "id": "call-1:stdout"}),
    )
    .await
    .expect("the search answers");
    assert!(found.contains("Matches in stored output (1)"), "{found}");
    assert!(found.contains("a needle here"), "{found}");
    let none = ask(
        &catalog,
        session,
        json!({"op": "search", "query": "absent", "id": "call-1:stdout"}),
    )
    .await
    .expect("the search answers");
    assert!(none.contains("No match in the stored output."), "{none}");
}

/// A search whose page filled up says the scan stopped and how to continue,
/// so a reader does not take a full page for the whole answer.
#[tokio::test]
async fn test_search_partial_says_more() {
    let (log, session) = seeded(
        b"needle needle needle needle needle needle needle needle needle needle needle needle",
        b"",
    );
    let catalog = SessionResultCatalog::new(log);
    let page = ask(
        &catalog,
        session,
        json!({"op": "search", "query": "needle", "id": "call-1:stdout"}),
    )
    .await
    .expect("the search answers");
    assert!(page.contains("Matches in stored output (10)"), "{page}");
    assert!(page.contains("The scan stopped at its budget"), "{page}");
    assert!(page.contains("Continue with cursor:"), "{page}");
}

/// The aggregate reports what the session kept, in the same terms the listing
/// uses, so a reader can size the whole before paging it.
#[tokio::test]
async fn test_stats_reports_totals() {
    let (log, session) = seeded(b"abc", b"z");
    let catalog = SessionResultCatalog::new(log);
    let stats = ask(&catalog, session, json!({"op": "stats"}))
        .await
        .expect("the aggregate answers");
    assert!(stats.contains("- results: 1"), "{stats}");
    assert!(
        stats.contains("- streams: 2 (2 complete, 0 truncated, 0 unknown)"),
        "{stats}"
    );
    assert!(stats.contains("- stdout: 3 B"), "{stats}");
    assert!(stats.contains("stderr: 1 B"), "{stats}");
    assert!(stats.contains("- kept between "), "{stats}");
}

/// A result the fold could not read is counted as unreadable and makes the
/// figures a floor, rather than vanishing as a result never kept.
#[tokio::test]
async fn test_stats_reports_unreadable() {
    let log = store();
    let session = SessionId::new();
    seed_missing_block(&log, session);
    let catalog = SessionResultCatalog::new(log);
    let stats = ask(&catalog, session, json!({"op": "stats"}))
        .await
        .expect("the aggregate answers");
    assert!(
        stats.contains("- unreadable: 1 results, 0 log bytes"),
        "{stats}"
    );
    assert!(stats.contains("the figures are a floor"), "{stats}");
}

/// A stored field whose block states no size is counted and named, so the byte
/// totals are not read as a complete measure of what the session kept.
#[tokio::test]
async fn test_stats_reports_unmeasured() {
    let log = store();
    let session = SessionId::new();
    seed_block(&log, session, br#""kept text""#, Some("captured"));
    let catalog = SessionResultCatalog::new(log);
    let stats = ask(&catalog, session, json!({"op": "stats"}))
        .await
        .expect("the aggregate answers");
    assert!(
        stats.contains("- 1 stored stream states no size, so their bytes are not in the totals"),
        "{stats}"
    );
}

/// A block an earlier path stored is listed under the name it was stored by,
/// and says how much of the stream it kept only when that is known.
#[tokio::test]
async fn test_list_names_stored_shapes() {
    let log = store();
    let session = SessionId::new();
    seed_block(&log, session, br#"{"stdout":"hi"}"#, None);
    let catalog = SessionResultCatalog::new(log);
    let whole = ask(&catalog, session, json!({"op": "list"}))
        .await
        .expect("the listing answers");
    assert!(whole.contains("| whole output | "), "{whole}");
    assert!(
        whole.contains("| 15 B | completeness unknown"),
        "a text block states its size and no completeness: {whole}"
    );

    let log = store();
    let session = SessionId::new();
    seed_block(&log, session, br#""kept text""#, Some("captured"));
    let catalog = SessionResultCatalog::new(log);
    let field = ask(&catalog, session, json!({"op": "list"}))
        .await
        .expect("the listing answers");
    assert!(field.contains("| field captured | "), "{field}");
    assert!(
        field.contains("| size unknown |"),
        "an escaped string states no size: {field}"
    );
}
