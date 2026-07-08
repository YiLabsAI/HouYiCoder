//! The external tool adapter: wraps a server subprocess speaking the
//! JSON-RPC 2.0 line protocol over stdio behind the Tool trait so its tools
//! plug into the registry like any local tool. The client is hand-rolled and
//! thin on purpose: initialize, tools/list, and tools/call are the only
//! methods the first cut needs, and a self-contained wire client keeps the
//! version isolation goal intact (no heavyweight SDK dependency that
//! could pin a shifting spec version or drag a runtime). The wire envelope is
//! pure and unit-tested with in-memory cursors; a real-server end-to-end test
//! is deferred until a stub binary is worth the build cost.
//!
//! v1 simplifications, by design:
//! - block-on-init: the composition root spawns the server and runs
//!   initialize plus tools/list BEFORE the agent loop starts, so the tool
//!   list is fixed for the session. A server that adds tools mid-session does
//!   not surface until restart.
//! - ignore listChanged: the listChanged notification is dropped on the
//!   floor (notifications are skipped while reading responses). Dynamic
//!   re-scan is a later cut.

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use serde_json::{Value, json};

use crate::launcher::{LauncherChild, ProcessLauncher, SpawnPolicy, SpawnRequest};
use crate::tool::{Tool, ToolCtx, ToolProvider};

/// The protocol version the client advertises on initialize. Pinned to a
/// stable spec revision; a future cut can negotiate up.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// A cached entry from tools/list: the name, description, and input schema
/// the model sees for one remote tool. The entry is immutable for the
/// session (block-on-init) so a tool adapter is just these three fields plus
/// a handle to the shared client.
#[derive(Clone)]
pub struct McpToolEntry {
    name: String,
    description: String,
    input_schema: Value,
}

impl McpToolEntry {
    /// The tool name the model addresses in a tool call.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Failures the client can report. Kept as an enum so a caller branches on
/// recovery (retry on a transient io failure, surface a protocol mismatch).
#[derive(Debug)]
pub enum McpError {
    /// A spawn, read, or write failure on the child pipes.
    Io(String),
    /// The server returned a JSON-RPC error response.
    ServerError { code: i64, message: String },
    /// A response was not valid JSON or did not match the expected shape.
    Protocol(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "mcp io failure: {m}"),
            Self::ServerError { code, message } => {
                write!(f, "mcp server error [{code}]: {message}")
            }
            Self::Protocol(m) => write!(f, "mcp protocol failure: {m}"),
        }
    }
}

impl std::error::Error for McpError {}

impl From<McpError> for ToolError {
    fn from(err: McpError) -> Self {
        Self::Failed(err.to_string())
    }
}

/// A live connection to a server subprocess. Owns the child handle (so
/// dropping it terminates the child via the launcher's on-drop kill) plus
/// the stdin and stdout pipes the client drives in a request-response cycle.
/// All access is through Arc plus Mutex so tool adapters sharing one client
/// serialize their calls — a v1 simplification (a later cut can pipeline or
/// multiplex on a single connection).
pub struct McpClient {
    /// Kept so dropping the client drops the child, triggering the on-drop
    /// kill the launcher installed. Wrapped in a Mutex so the client is Sync
    /// (the child's boxed wait future is Send but not Sync on its own); the
    /// field is never locked, only dropped.
    _child: Mutex<LauncherChild>,
    stdin: Arc<Mutex<BufWriter<Box<dyn Write + Send>>>>,
    stdout: Arc<Mutex<BufReader<Box<dyn Read + Send>>>>,
    next_id: AtomicU64,
}

impl McpClient {
    /// Spawn a server subprocess via the launcher, run the initialize plus
    /// tools/list handshake synchronously, and return the client plus the
    /// cached tool list. Blocking on init is the v1 contract: the caller
    /// runs this before the agent loop starts, so the session has a fixed
    /// tool list. The launcher must produce live pipe handles (an interactive
    /// spawn); a capture-on-exit spawn returns no handles and fails init.
    pub fn spawn(
        program: &str,
        args: &[String],
        launcher: &dyn ProcessLauncher,
    ) -> Result<(Self, Vec<McpToolEntry>), McpError> {
        let mut req = SpawnRequest::new(program)
            .with_args(args.iter().cloned())
            .interactive();
        // Stderr to null, not piped: a server that logs to stderr would
        // SIGPIPE-crash when this client drops the read end of a piped
        // stderr (broken pipe on its first log line after the buffer fills).
        // Null lets the server discard its logs without a reader.
        req.stdio.stderr = crate::launcher::StdioMode::Null;
        let mut child = launcher
            .spawn(req, SpawnPolicy::default().audited())
            .map_err(|e| McpError::Io(e.to_string()))?;
        let pipes = child
            .take_pipes()
            .ok_or_else(|| McpError::Protocol("interactive spawn returned no pipes".into()))?;
        let stdin = pipes
            .stdin
            .ok_or_else(|| McpError::Protocol("server stdin not piped".into()))?;
        let stdout = pipes
            .stdout
            .ok_or_else(|| McpError::Protocol("server stdout not piped".into()))?;
        // stderr is dropped here: server log lines are discarded in the first
        // cut. A later cut can tee them to the host log.
        drop(pipes.stderr);
        let client = Self {
            _child: Mutex::new(child),
            stdin: Arc::new(Mutex::new(BufWriter::new(stdin))),
            stdout: Arc::new(Mutex::new(BufReader::new(stdout))),
            next_id: AtomicU64::new(1),
        };
        client.initialize()?;
        let tools = client.list_tools()?;
        Ok((client, tools))
    }

    /// Build a client from already-spawned pipe handles. Used by tests so
    /// the wire logic can be exercised against an in-memory transport without
    /// spawning a real subprocess.
    #[cfg(test)]
    fn from_pipes(
        stdin: Box<dyn Write + Send>,
        stdout: Box<dyn Read + Send>,
        child: LauncherChild,
    ) -> Self {
        Self {
            _child: Mutex::new(child),
            stdin: Arc::new(Mutex::new(BufWriter::new(stdin))),
            stdout: Arc::new(Mutex::new(BufReader::new(stdout))),
            next_id: AtomicU64::new(1),
        }
    }

    /// The initialize handshake: send initialize with the client info and
    /// protocol version, read the result, then send the initialized
    /// notification (no response expected). Must be the first call after
    /// spawn; the server refuses tools/list before it sees initialize.
    fn initialize(&self) -> Result<Value, McpError> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "houyicoder", "version": env!("CARGO_PKG_VERSION") }
        });
        let result = self.round_trip("initialize", params)?;
        // Send the initialized notification. It carries no id and expects no
        // response; the server processes it silently.
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        self.write_line(&notification)?;
        Ok(result)
    }

    /// Call tools/list and cache the entries. Called once at spawn; the v1
    /// block-on-init contract means the list is fixed for the session and the
    /// listChanged notification is ignored. Follows the nextCursor pagination
    /// a server may apply (the spec lets a server page the tool list); a
    /// server that returns all tools in one response has no nextCursor and the
    /// loop runs once.
    fn list_tools(&self) -> Result<Vec<McpToolEntry>, McpError> {
        let mut all: Vec<McpToolEntry> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self.round_trip("tools/list", params)?;
            let tools = result
                .get("tools")
                .cloned()
                .ok_or_else(|| McpError::Protocol("tools/list response missing tools".into()))?;
            let arr = tools
                .as_array()
                .ok_or_else(|| McpError::Protocol("tools/list tools field not an array".into()))?;
            for entry in arr.iter() {
                all.push(parse_tool_entry(entry)?);
            }
            cursor = result
                .get("nextCursor")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string());
            if cursor.is_none() {
                break;
            }
        }
        Ok(all)
    }

    /// Send a tools/call request and return the result payload. Acquires the
    /// stdin and stdout locks for the whole round trip so a concurrent caller
    /// does not interleave its request with another's response. Blocking on
    /// a runtime thread is correct here because the caller (the agent loop) is
    /// already on a runtime; the public tool adapter wraps this in a
    /// runtime-blocking task so the executor thread is not stalled.
    pub fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        let params = json!({ "name": name, "arguments": arguments });
        self.round_trip("tools/call", params)
    }

    /// The request-response core: mint the next id, build the envelope, write
    /// it as one line, then read lines until a response with the matching id
    /// arrives. Notifications (no id, or a method without an id) are skipped
    /// so listChanged and progress do not confuse the reader.
    fn round_trip(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = build_request(id, method, params);
        let mut stdout = self.stdout.lock().expect("mutex poisoned");
        let mut stdin = self.stdin.lock().expect("mutex poisoned");
        // Write the request then flush so the server sees a full line; without
        // the flush the BufWriter holds the bytes until its buffer fills.
        stdin
            .write_all(request.as_bytes())
            .map_err(|e| McpError::Io(e.to_string()))?;
        stdin.flush().map_err(|e| McpError::Io(e.to_string()))?;
        drop(stdin);
        // Deref the guard to a dyn BufRead so read_response can drive the
        // blocking line read. The guard lives to the end of the function so
        // no concurrent caller can interleave a request between our write and
        // our read.
        let mut reader: &mut dyn BufRead = &mut *stdout;
        read_response(&mut reader, id)
    }

    fn write_line(&self, value: &Value) -> Result<(), McpError> {
        let mut stdin = self.stdin.lock().expect("mutex poisoned");
        let line = serde_json::to_string(value).map_err(|e| McpError::Protocol(e.to_string()))?;
        stdin
            .write_all(line.as_bytes())
            .map_err(|e| McpError::Io(e.to_string()))?;
        stdin
            .write_all(b"\n")
            .map_err(|e| McpError::Io(e.to_string()))?;
        stdin.flush().map_err(|e| McpError::Io(e.to_string()))?;
        Ok(())
    }
}

/// One tool entry from the tools/list response. Extracts the name,
/// description, and inputSchema fields; fails if name is missing or not a
/// string (a tool the model cannot address is useless). description and
/// inputSchema default to empty / object when absent so a minimal server
/// entry still loads.
fn parse_tool_entry(v: &Value) -> Result<McpToolEntry, McpError> {
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| McpError::Protocol("tool entry missing string name".into()))?
        .to_string();
    let description = v
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    let input_schema = v.get("inputSchema").cloned().unwrap_or(json!({}));
    Ok(McpToolEntry {
        name,
        description,
        input_schema,
    })
}

/// Build a JSON-RPC 2.0 request envelope as one line (terminated by a
/// newline) for the given id, method, and params. Pure so the wire format is
/// testable without a live subprocess.
fn build_request(id: u64, method: &str, params: Value) -> String {
    let envelope = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    });
    let mut line = serde_json::to_string(&envelope).expect("envelope is serializable");
    line.push('\n');
    line
}

/// Read lines from the reader until a JSON-RPC response with the expected id
/// arrives. Lines that are notifications (no id field, or a method without an
/// id) are skipped — the v1 contract ignores listChanged and progress. A
/// response with a mismatched id is a protocol error (the client sends one
/// request at a time, so the server should not emit a stray id). On a
/// JSON-RPC error response, returns ServerError. On a result response,
/// returns the result value.
fn read_response(reader: &mut dyn BufRead, expected_id: u64) -> Result<Value, McpError> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| McpError::Io(e.to_string()))?;
        if n == 0 {
            return Err(McpError::Io("server stdout closed before response".into()));
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| McpError::Protocol(format!("response not valid json: {e}")))?;
        // A notification has a method but no id; skip it (listChanged,
        // progress, initialized). The v1 contract ignores these.
        if value.get("id").is_none() && value.get("method").is_some() {
            continue;
        }
        let id = value
            .get("id")
            .and_then(|i| i.as_u64())
            .ok_or_else(|| McpError::Protocol("response missing numeric id".into()))?;
        if id != expected_id {
            return Err(McpError::Protocol(format!(
                "response id {id} does not match expected {expected_id}"
            )));
        }
        if let Some(err) = value.get("error") {
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return Err(McpError::ServerError { code, message });
        }
        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
    }
}

/// A remote tool behind the Tool trait. Holds a shared handle to the client
/// and the cached entry from tools/list. execute sends a tools/call through
/// the client on a runtime-blocking task so the executor thread is not
/// stalled by the blocking pipe read.
pub struct McpTool {
    client: Arc<McpClient>,
    entry: McpToolEntry,
}

impl McpTool {
    fn new(client: Arc<McpClient>, entry: McpToolEntry) -> Self {
        Self { client, entry }
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.entry.name
    }

    fn description(&self) -> &str {
        &self.entry.description
    }

    fn input_schema(&self) -> Value {
        self.entry.input_schema.clone()
    }

    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let client = self.client.clone();
        let name = self.entry.name.clone();
        Box::pin(async move {
            // The client uses blocking pipe I/O; run it on a runtime-blocking
            // thread so the async executor is not stalled. Errors map to
            // Failed so the model sees them and the loop continues.
            let result = tokio::task::spawn_blocking(move || client.call_tool(&name, input))
                .await
                .map_err(|e| ToolError::Failed(format!("tool task join failed: {e}")))?;
            Ok(result?)
        })
    }

    // Fail-closed defaults inherited from the trait: a remote tool is assumed
    // mutating, destructive, and non-concurrent unless a future cut learns
    // otherwise from server hints. Approval is left to the host gate that
    // wraps the adapter (the same GuardedTool path the built-in tools take).
}

/// A ToolProvider that contributes one server's tools to the registry. Built
/// from a spawned client plus the cached tool list; tools() returns one
/// adapter per entry, all sharing the one client (so calls serialize through
/// its locks — a v1 simplification).
pub struct McpToolProvider {
    client: Arc<McpClient>,
    entries: Vec<McpToolEntry>,
}

impl McpToolProvider {
    /// Construct from the spawned client and cached entries. The composition
    /// root calls this after spawn; it holds the only client handle so all
    /// tool adapters share one subprocess.
    pub fn new(client: McpClient, entries: Vec<McpToolEntry>) -> Self {
        Self {
            client: Arc::new(client),
            entries,
        }
    }
}

impl ToolProvider for McpToolProvider {
    fn name(&self) -> &str {
        "mcp"
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.entries
            .iter()
            .map(|e| Arc::new(McpTool::new(self.client.clone(), e.clone())) as Arc<dyn Tool>)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
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
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
