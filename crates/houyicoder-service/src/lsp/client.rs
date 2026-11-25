//! One live language server connection: the handshake, the multiplexer that
//! routes answers by id, the reverse requests a server may send, and the
//! progress notifications that say whether indexing finished. One writer
//! task owns the outgoing half so two requests never interleave inside a
//! frame; one reader task dispatches every incoming frame. A request
//! answered with the state-changed error is retried on a doubling schedule,
//! because an indexing server answers that way for a while and then starts
//! working.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Display;
use std::fmt::Formatter;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value;
use serde_json::json;
use tokio::io::DuplexStream;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio::time::sleep;
use tokio::time::timeout;

use super::framing::read_frame;
use super::framing::write_frame;
use crate::child_process::file_uri;

/// The request error code a server answers with while its state moved under
/// the request, which is what an indexing server does until it settles.
pub(crate) const CONTENT_MODIFIED: i64 = -32801;

/// How long each retry after a transient error waits; the last value
/// repeats for any further attempt.
pub(crate) type RetryDelays = [Duration; 3];

/// The production retry schedule: half a second doubling to two seconds.
pub(crate) const DEFAULT_RETRY_DELAYS: RetryDelays = [
    Duration::from_millis(500),
    Duration::from_millis(1_000),
    Duration::from_millis(2_000),
];

/// How many retries a transient error spends before it surfaces.
const MAX_RETRIES: u32 = 3;

/// The answer a request resolves with: the result value, or the server's
/// structured error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
}

impl Display for RpcError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

/// One notification the server published, forwarded to its consumer.
#[derive(Debug, Clone)]
pub(crate) struct ServerNotification {
    pub method: String,
    pub params: Value,
}

/// The state the reader task and the handle share.
struct ClientState {
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>,
    /// The progress tokens currently open; a connection is indexing while
    /// its set is non-empty.
    progress: Mutex<HashSet<String>>,
    index_message: Mutex<Option<String>>,
    closed: AtomicBool,
    closed_notify: Notify,
}

/// One connection's handle. Cloning is not supported: the owner keeps an
/// Arc when several callers address one server.
pub(crate) struct LspClient {
    /// Framed bodies out to the writer task.
    outbound: mpsc::Sender<Vec<u8>>,
    next_id: AtomicU64,
    state: Arc<ClientState>,
    retry_delays: RetryDelays,
}

impl LspClient {
    /// Start the reader and writer tasks over one pair of streams and return
    /// the handle plus the notification stream the owner drains.
    pub(crate) fn spawn(
        reader: DuplexStream,
        writer: DuplexStream,
        retry_delays: RetryDelays,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<ServerNotification>) {
        let (outbound, out_rx) = mpsc::channel::<Vec<u8>>(64);
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
        let state = Arc::new(ClientState {
            pending: Mutex::new(HashMap::new()),
            progress: Mutex::new(HashSet::new()),
            index_message: Mutex::new(None),
            closed: AtomicBool::new(false),
            closed_notify: Notify::new(),
        });
        let client = Arc::new(Self {
            outbound: outbound.clone(),
            next_id: AtomicU64::new(1),
            state: Arc::clone(&state),
            retry_delays,
        });
        tokio::spawn(write_loop(writer, out_rx));
        tokio::spawn(read_loop(reader, outbound, state, notice_tx));
        (client, notice_rx)
    }

    /// Send one request and wait for its answer, retrying the transient
    /// state-changed error on the doubling schedule.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut attempt = 0u32;
        loop {
            let outcome = self.request_once(method, params.clone()).await;
            match outcome {
                Err(error) if error.code == CONTENT_MODIFIED && attempt < MAX_RETRIES => {
                    let delay = self.retry_delays[(attempt as usize).min(2)];
                    attempt += 1;
                    sleep(delay).await;
                }
                other => return other,
            }
        }
    }

    /// One request attempt: mint an id, register the answer slot, write the
    /// frame, and await the slot.
    async fn request_once(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.state.pending.lock() {
            pending.insert(id, tx);
        }
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let body = match serde_json::to_vec(&frame) {
            Ok(body) => body,
            Err(error) => {
                remove_pending(&self.state, id);
                return Err(RpcError {
                    code: 0,
                    message: error.to_string(),
                });
            }
        };
        if self.outbound.send(body).await.is_err() {
            remove_pending(&self.state, id);
            return Err(closed_error());
        }
        let _guard = PendingGuard {
            state: Arc::clone(&self.state),
            id,
        };
        match rx.await {
            Ok(answer) => answer,
            Err(_) => Err(closed_error()),
        }
    }

    /// Send one notification, fire and forget.
    pub(crate) fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let body = serde_json::to_vec(&frame).map_err(|error| error.to_string())?;
        self.outbound.try_send(body).map_err(|error| match error {
            TrySendError::Full(_) => "the send queue is full".to_string(),
            TrySendError::Closed(_) => "the connection is closed".to_string(),
        })
    }

    /// The handshake: initialize, then the initialized notification.
    pub(crate) async fn initialize(
        &self,
        params: Value,
        budget: Duration,
    ) -> Result<Value, String> {
        let answer = timeout(budget, self.request("initialize", params))
            .await
            .map_err(|_| "the handshake did not answer inside its budget".to_string())?
            .map_err(|error| format!("the handshake was refused: {error}"))?;
        self.notify("initialized", json!({}))?;
        Ok(answer)
    }

    /// The graceful stop: shutdown bounded by its budget, then exit. A
    /// server that does not answer in time is dropped by the caller.
    pub(crate) async fn shutdown(&self, budget: Duration) {
        if self.state.closed.load(Ordering::Relaxed) {
            return;
        }
        drop(timeout(budget, self.request("shutdown", Value::Null)).await);
        drop(self.notify("exit", Value::Null));
    }

    /// Whether the server reports background work in progress.
    pub(crate) fn is_indexing(&self) -> bool {
        self.state
            .progress
            .lock()
            .map(|tokens| !tokens.is_empty())
            .unwrap_or(false)
    }

    /// The headline of the newest open progress token, when one carries it.
    pub(crate) fn index_message(&self) -> Option<String> {
        self.state
            .index_message
            .lock()
            .ok()
            .and_then(|message| message.clone())
    }

    /// Whether the connection has ended.
    pub(crate) fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Relaxed)
    }

    /// Resolves once the connection ends, so the owner can decide whether to
    /// relaunch.
    pub(crate) async fn wait_closed(&self) {
        loop {
            if self.is_closed() {
                return;
            }
            self.state.closed_notify.notified().await;
        }
    }
}

/// Removes one registered answer slot when the request future goes away,
/// so a caller cancelled mid-flight (a timeout fires) leaves no entry
/// behind for a reply that may never come.
struct PendingGuard {
    state: Arc<ClientState>,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        remove_pending(&self.state, self.id);
    }
}

/// Drop one registered answer slot.
fn remove_pending(state: &ClientState, id: u64) {
    if let Ok(mut pending) = state.pending.lock() {
        pending.remove(&id);
    }
}

/// The error every abandoned request resolves with once the stream ends.
fn closed_error() -> RpcError {
    RpcError {
        code: -32099,
        message: "the connection closed before the answer arrived".to_string(),
    }
}

/// The writer task: frame each queued body onto the stream.
async fn write_loop(mut writer: DuplexStream, mut out_rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(body) = out_rx.recv().await {
        if write_frame(&mut writer, &body).await.is_err() {
            return;
        }
    }
}

/// The reader task: dispatch each frame, answer server requests, track
/// progress, forward notifications, and settle every waiter on the way out.
async fn read_loop(
    mut reader: DuplexStream,
    outbound: mpsc::Sender<Vec<u8>>,
    state: Arc<ClientState>,
    notice_tx: mpsc::UnboundedSender<ServerNotification>,
) {
    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(Some(body)) => body,
            _ => break,
        };
        let Ok(value) = serde_json::from_slice::<Value>(&frame) else {
            continue;
        };
        dispatch(&value, &outbound, &state, &notice_tx);
    }
    state.closed.store(true, Ordering::Relaxed);
    if let Ok(mut pending) = state.pending.lock() {
        for (_, tx) in pending.drain() {
            drop(tx.send(Err(closed_error())));
        }
    }
    // notify_one stores a permit, so a watcher that checks the flag and
    // starts waiting between the store above and this call cannot miss it.
    state.closed_notify.notify_one();
}

/// Route one decoded frame by its shape: a response, a server request, or a
/// notification.
fn dispatch(
    value: &Value,
    outbound: &mpsc::Sender<Vec<u8>>,
    state: &ClientState,
    notice_tx: &mpsc::UnboundedSender<ServerNotification>,
) {
    let method = value.get("method").and_then(Value::as_str);
    let id = value.get("id");
    match (method, id) {
        (Some(method), Some(id)) => answer_server_request(outbound, method, id, value),
        (Some(method), None) => {
            if method == "$/progress" {
                track_progress(state, value);
            }
            drop(notice_tx.send(ServerNotification {
                method: method.to_string(),
                params: value.get("params").cloned().unwrap_or(Value::Null),
            }));
        }
        (None, Some(id)) => settle_response(state, id, value),
        (None, None) => {}
    }
}

/// Complete the waiter one response addresses.
fn settle_response(state: &ClientState, id: &Value, value: &Value) {
    let Some(id) = id.as_u64() else {
        return;
    };
    let sender = state
        .pending
        .lock()
        .ok()
        .and_then(|mut map| map.remove(&id));
    let Some(sender) = sender else {
        return;
    };
    if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the request failed")
            .to_string();
        drop(sender.send(Err(RpcError { code, message })));
    } else {
        drop(sender.send(Ok(value.get("result").cloned().unwrap_or(Value::Null))));
    }
}

/// The three reverse requests a server actually sends, and a refusal for
/// the rest. Configuration answers nulls: the host has none to give, and a
/// server treats an absent value as its own default.
fn answer_server_request(
    outbound: &mpsc::Sender<Vec<u8>>,
    method: &str,
    id: &Value,
    value: &Value,
) {
    let reply = match method {
        "workspace/configuration" => {
            let count = value
                .get("params")
                .and_then(|params| params.get("items"))
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            json!({"jsonrpc": "2.0", "id": id, "result": vec![Value::Null; count]})
        }
        "window/workDoneProgress/create" | "client/registerCapability" => {
            json!({"jsonrpc": "2.0", "id": id, "result": Value::Null})
        }
        "workspace/applyEdit" => {
            json!({"jsonrpc": "2.0", "id": id,
                "result": {"applied": false, "failureReason": "edits from the language server are not applied"}})
        }
        _ => json!({"jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("unsupported request {method}")}}),
    };
    match serde_json::to_vec(&reply) {
        Ok(body) => {
            if outbound.try_send(body).is_err() {
                tracing::warn!(
                    method,
                    "a server request could not be answered; the queue is full or closed"
                );
            }
        }
        Err(error) => tracing::warn!(%error, "a server answer could not be encoded"),
    }
}

/// Track the work-done progress tokens: begin opens one, end closes it, and
/// a report refreshes the headline. The connection reads as indexing while
/// any token is open.
fn track_progress(state: &ClientState, value: &Value) {
    let Some(params) = value.get("params") else {
        return;
    };
    let token = params
        .get("token")
        .map(|token| token.to_string())
        .unwrap_or_default();
    let progress = params.get("value");
    let kind = progress
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let headline = progress
        .and_then(|value| value.get("message").or_else(|| value.get("title")))
        .and_then(Value::as_str)
        .map(str::to_string);
    match kind {
        "begin" => {
            if let Ok(mut tokens) = state.progress.lock() {
                tokens.insert(token);
            }
            if let (Some(headline), Ok(mut message)) = (headline, state.index_message.lock()) {
                *message = Some(headline);
            }
        }
        "end" => {
            if let Ok(mut tokens) = state.progress.lock() {
                tokens.remove(&token);
            }
            if state
                .progress
                .lock()
                .map(|tokens| tokens.is_empty())
                .unwrap_or(true)
                && let Ok(mut message) = state.index_message.lock()
            {
                *message = None;
            }
        }
        _ => {
            if let (Some(headline), Ok(mut message)) = (headline, state.index_message.lock()) {
                *message = Some(headline);
            }
        }
    }
}

/// The initialize parameters every server receives: the identity fields
/// all servers need, the modern workspace-folder spelling and the older URI
/// spelling that some still read, the capability matrix the tool relies on,
/// and the progress capability without which an indexing server stays
/// silent about its work.
pub(crate) fn initialize_params(workspace: &Path, options: Option<&Value>) -> Value {
    let uri = file_uri(workspace);
    let name = workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    json!({
        "processId": std::process::id(),
        "clientInfo": {"name": "houyicoder"},
        "rootPath": workspace.to_string_lossy(),
        "rootUri": uri,
        "workspaceFolders": [{"uri": uri, "name": name}],
        "initializationOptions": options.cloned().unwrap_or_else(|| json!({})),
        "capabilities": {
            "workspace": {"configuration": false, "workspaceFolders": false},
            "textDocument": {
                "synchronization": {"didSave": true},
                "publishDiagnostics": {"relatedInformation": true},
                "hover": {"contentFormat": ["markdown", "plaintext"]},
                "definition": {"linkSupport": true},
                "implementation": {},
                "references": {},
                "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                "callHierarchy": {}
            },
            "general": {"positionEncodings": ["utf-16"]},
            "window": {"workDoneProgress": true}
        }
    })
}

#[cfg(test)]
pub(crate) mod tests;
