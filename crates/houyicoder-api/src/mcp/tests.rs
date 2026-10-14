//! Tests for this module.

use super::*;
use std::io::Cursor;
use std::io::Error;
use std::sync::mpsc;

/// Build a request line and prove the envelope has the id, method, and
/// params in the right places plus the trailing newline the reader
/// expects.
#[test]
fn test_build_request_envelope() {
    let line = build_request(7, "tools/list", json!({}));
    let v: Value = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 7);
    assert_eq!(v["method"], "tools/list");
    assert!(line.ends_with('\n'));
}

/// A result response with the matching id returns its result value.
#[test]
fn test_read_response_result() {
    let raw = "{\"id\":3,\"result\":{\"tools\":[]}}\n";
    let mut reader = Cursor::new(raw.as_bytes());
    let result = read_response(&mut reader, 3).unwrap();
    assert_eq!(result["tools"], json!([]));
}

/// A JSON-RPC error response surfaces as a ServerError with the code and
/// message the server sent.
#[test]
fn test_read_response_error() {
    let raw = "{\"id\":1,\"error\":{\"code\":-32601,\"message\":\"no method\"}}\n";
    let mut reader = Cursor::new(raw.as_bytes());
    let err = read_response(&mut reader, 1).unwrap_err();
    match err {
        McpError::ServerError { code, message } => {
            assert_eq!(code, -32601);
            assert_eq!(message, "no method");
        }
        other => panic!("expected ServerError, got {other:?}"),
    }
}

/// A notification has a method but no id; the reader skips it and returns
/// the next real response. This is the listChanged plus progress skip
/// path.
#[test]
fn test_read_response_skips_notification() {
    let raw = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\
               {\"id\":2,\"result\":{\"ok\":true}}\n";
    let mut reader = Cursor::new(raw.as_bytes());
    let result = read_response(&mut reader, 2).unwrap();
    assert_eq!(result["ok"], true);
}

/// A response whose id does not match the expected id is a protocol
/// error: the client sends one request at a time, so a stray id means the
/// frames are misaligned.
#[test]
fn test_read_response_mismatched_id() {
    let raw = "{\"id\":5,\"result\":{}}\n";
    let mut reader = Cursor::new(raw.as_bytes());
    let err = read_response(&mut reader, 9).unwrap_err();
    assert!(matches!(err, McpError::Protocol(_)));
}

/// A closed stdout before any response is an io failure, not a hang.
#[test]
fn test_read_response_eof() {
    let mut reader = Cursor::new(b"");
    let err = read_response(&mut reader, 1).unwrap_err();
    assert!(matches!(err, McpError::Io(_)));
}

/// A tools/list entry with a string name and an inputSchema loads.
#[test]
fn test_parse_entry_ok() {
    let v = json!({
        "name": "search",
        "description": "search files",
        "inputSchema": {"type": "object"}
    });
    let e = parse_tool_entry(&v).unwrap();
    assert_eq!(e.name(), "search");
    assert_eq!(e.description, "search files");
    assert_eq!(e.input_schema["type"], "object");
}

/// A tool entry without a string name is rejected.
#[test]
fn test_parse_entry_missing_name() {
    let v = json!({"description": "no name"});
    assert!(parse_tool_entry(&v).is_err());
}

/// The adapter round-trips a stub response through the wire logic: a
/// tools/call against an in-memory transport returns the result payload
/// the stub scripted. Proves the client plus adapter compose end to end
/// without a real subprocess.
#[test]
fn test_adapter_round_trips_stub() {
    // A stub server script: one canned line per read. The initialize
    // response, the tools/list response, then the tools/call response.
    let init_resp = "{\"id\":1,\"result\":{\"protocolVersion\":\"2024-11-05\"}}\n".to_string();
    let list_resp =
        "{\"id\":2,\"result\":{\"tools\":[{\"name\":\"echo\",\"description\":\"echo\"}]}}\n"
            .to_string();
    let call_resp =
        "{\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"isError\":false}}\n"
            .to_string();
    let server_buf = format!("{init_resp}{list_resp}{call_resp}");
    let stdout: Box<dyn Read + Send> = Box::new(Cursor::new(server_buf.into_bytes()));
    // stdin sink: collect into a Vec via a channel so we can inspect what
    // the client sent. The BufWriter flushes per call.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let stdin: Box<dyn Write + Send> = Box::new(ChannelSink { tx });
    let stub_child = LauncherChild::new(None, Box::pin(async { Ok(Default::default()) }));
    let client = McpClient::from_pipes(stdin, stdout, stub_child);
    // initialize plus tools/list run at spawn-time in the real path; here
    // we call them explicitly to drive the stub against the same logic.
    let init = client.initialize().unwrap();
    assert_eq!(init["protocolVersion"], "2024-11-05");
    let entries = client.list_tools().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "echo");
    // The next id is 3 after initialize (1) plus list (2); the stub
    // scripts id 3 for the call, so this matches.
    let result = client.call_tool("echo", json!({"x": 1})).unwrap();
    assert_eq!(result["content"][0]["text"], "hi");
    assert_eq!(result["isError"], false);
    // The client should have written three request lines plus the
    // initialized notification. Inspect the captured stdin.
    let sent: Vec<u8> = rx.try_iter().flatten().collect();
    let sent_str = String::from_utf8(sent).unwrap();
    assert!(sent_str.contains("\"method\":\"initialize\""));
    assert!(sent_str.contains("\"notifications/initialized\""));
    assert!(sent_str.contains("\"method\":\"tools/list\""));
    assert!(sent_str.contains("\"method\":\"tools/call\""));
}

/// A channel-backed Write sink so the test can capture what the client
/// wrote to stdin without a real pipe.
struct ChannelSink {
    tx: mpsc::Sender<Vec<u8>>,
}

impl Write for ChannelSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tx
            .send(buf.to_vec())
            .map_err(|e| Error::other(e.to_string()))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
