//! Replay of a stream recorded under a double-byte code page. The parent
//! module gates this file to the host that has the code-page table; the test
//! reads real characters, so it runs only where decoding is real.

use std::sync::Arc;

use houyicoder_api::capture::{CallEnd, CapturedStream, CapturedStreams, ResultCaptureSource};
use houyicoder_api::result_catalog::ResultCatalog;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_platform::host::text::OutputEncoding;
use houyicoder_protocol::result_catalog::{ResultContent, ResultId, ResultRange, ResultReadQuery};
use serde_json::json;

use crate::agent::tools::result_capture::SessionResultCapture;

use super::SessionResultCatalog;
use super::{entry, store};

/// The GBK bytes of the character the stream repeats.
const GBK_CHAR: [u8; 2] = [0xd6, 0xd0];

/// A read that starts inside a run of double-byte characters still answers
/// whole characters. Counting back a fixed three bytes lands on a trail byte
/// here, and every character of the page would decode shifted; the window
/// resumes after the last single-byte character instead.
#[tokio::test]
async fn test_code_page_stays_aligned() {
    let log = store();
    let session = SessionId::new();
    // A newline anchors the stream, then a run of one double-byte character
    // long enough to cross a window-sized read.
    let mut bytes = vec![b'\n'];
    for _ in 0..40_000 {
        bytes.extend(GBK_CHAR);
    }
    seed(&log, session, &bytes);
    let catalog = SessionResultCatalog::new(Arc::clone(&log));
    let read = catalog
        .read(
            session,
            ResultReadQuery {
                id: ResultId::new("call-1:stdout"),
                range: ResultRange::Bytes {
                    offset: 65_535,
                    len: 512,
                },
                cursor: None,
            },
        )
        .await
        .expect("the byte read answers");
    let ResultContent::Text { text } = read.content else {
        panic!("a text stream reads as text");
    };
    assert!(!text.contains('\u{fffd}'), "no character is cut: {text:?}");
    assert!(
        text.chars().all(|ch| ch == '\n' || ch == '\u{4e2d}'),
        "every character is the one the producer wrote: {:?}",
        text.chars().take(8).collect::<String>()
    );
}

/// Publish a capture whose interpreter declared the GBK code page.
fn seed(log: &Arc<dyn SessionLog>, session: SessionId, stdout: &[u8]) {
    let source = SessionResultCapture::new(Arc::clone(log));
    let capture = source
        .begin(
            session,
            "call-1",
            "cmd",
            OutputEncoding::LegacyCodePage(936),
        )
        .expect("a capture source starts a capture");
    capture.receive(CapturedStream::Stdout, stdout);
    let published: CapturedStreams = pollster::block_on(capture.finish(CallEnd::Exited(Some(0))))
        .expect("finish succeeds")
        .expect("output is published");
    let call = entry(
        session,
        SessionEvent::ToolCall {
            call_id: "call-1".to_string(),
            tool: "cmd".to_string(),
            input: json!({"command": "run"}),
        },
    );
    pollster::block_on(log.append(call)).expect("the call is logged");
    let result = entry(
        session,
        SessionEvent::ToolResult {
            call_id: "call-1".to_string(),
            output: json!({
                "raw_capture": {"record": {"block_ref": published.record.0}}
            }),
            duration_ms: 3,
        },
    );
    pollster::block_on(log.append(result)).expect("the result is logged");
}
