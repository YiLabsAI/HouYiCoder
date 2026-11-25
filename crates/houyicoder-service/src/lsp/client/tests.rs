//! Tests for the handshake parameters and the framed plumbing, driven over
//! an in-memory peer half that answers from a table.

use std::sync::Arc;

use tokio::io::duplex;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::Duration;
use tokio::time::sleep;
use tokio::time::timeout;

use super::*;
use crate::lsp::framing::read_frame;
use crate::lsp::framing::write_frame;

/// One framed peer over the other half of a duplex pair. The handler maps
/// each incoming message to the messages to answer with; the sender lets a
/// test push a message unprompted (a progress token, a diagnostic).
pub(crate) struct Peer {
    pub out: UnboundedSender<Value>,
    pub seen: Arc<std::sync::Mutex<Vec<Value>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        // Abort both tasks: the reader holds a reply sender, so the writer
        // task would otherwise keep the peer's stream half open and the
        // client would never see the end of its stream.
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

/// Start a peer over one half of a pair and return its handle.
pub(crate) fn spawn_peer(
    mut reader: DuplexStream,
    writer: DuplexStream,
    handler: impl Fn(&Value) -> Vec<Value> + Send + 'static,
) -> Peer {
    let (tx, mut rx) = unbounded_channel::<Value>();
    let writer = Arc::new(AsyncMutex::new(writer));
    let writer_task = Arc::clone(&writer);
    let write_handle = tokio::spawn(async move {
        while let Some(value) = rx.recv().await {
            let Ok(body) = serde_json::to_vec(&value) else {
                continue;
            };
            let mut writer = writer_task.lock().await;
            if write_frame(&mut *writer, &body).await.is_err() {
                return;
            }
        }
    });
    let out = tx.clone();
    let reply_tx = tx.clone();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_log = Arc::clone(&seen);
    let read_handle = tokio::spawn(async move {
        while let Ok(Some(body)) = read_frame(&mut reader).await {
            let Ok(value) = serde_json::from_slice::<Value>(&body) else {
                continue;
            };
            if let Ok(mut log) = seen_log.lock() {
                log.push(value.clone());
            }
            for reply in handler(&value) {
                if reply_tx.send(reply).is_err() {
                    return;
                }
            }
        }
    });
    Peer {
        out,
        seen,
        tasks: vec![write_handle, read_handle],
    }
}

/// A pair wired into one client plus its peer.
fn pair(handler: impl Fn(&Value) -> Vec<Value> + Send + 'static) -> (Arc<LspClient>, Peer) {
    let (client_read, server_write) = duplex(64 * 1024);
    let (server_read, client_write) = duplex(64 * 1024);
    let peer = spawn_peer(server_read, server_write, handler);
    let (client, _notices) = LspClient::spawn(
        client_read,
        client_write,
        [
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(4),
        ],
    );
    (client, peer)
}

/// The handshake carries the identity fields every server reads, both
/// workspace spellings, the capability matrix the tool relies on, and the
/// progress capability without which an indexing server stays silent.
#[test]
fn test_initialize_params_matrix() {
    let params = initialize_params(Path::new("/work/src"), None);
    assert_eq!(params["rootPath"], "/work/src");
    assert_eq!(params["rootUri"], "file:///work/src");
    assert_eq!(params["workspaceFolders"][0]["name"], "src");
    assert_eq!(
        params["initializationOptions"],
        json!({}),
        "an absent options value is an empty object, never null"
    );
    assert_eq!(params["capabilities"]["window"]["workDoneProgress"], true);
    assert_eq!(
        params["capabilities"]["general"]["positionEncodings"][0],
        "utf-16"
    );
    assert_eq!(
        params["capabilities"]["textDocument"]["documentSymbol"]["hierarchicalDocumentSymbolSupport"],
        true
    );
    assert_eq!(
        params["capabilities"]["textDocument"]["definition"]["linkSupport"],
        true
    );
    assert_eq!(
        params["capabilities"]["textDocument"]["synchronization"]["didSave"],
        true
    );

    let options = json!({"assist": {"enabled": true}});
    let with_options = initialize_params(Path::new("/work"), Some(&options));
    assert_eq!(with_options["initializationOptions"], options);
}

/// The progress tracker opens on begin, closes on end, and reports indexing
/// exactly while a token is open.
#[test]
fn test_progress_tracking() {
    let state = ClientState {
        pending: Mutex::new(HashMap::new()),
        progress: Mutex::new(HashSet::new()),
        index_message: Mutex::new(None),
        closed: AtomicBool::new(false),
        closed_notify: Notify::new(),
    };
    let begin =
        json!({"params": {"token": "t1", "value": {"kind": "begin", "message": "indexing"}}});
    track_progress(&state, &begin);
    assert!(state.progress.lock().expect("lock").contains("\"t1\""));
    assert_eq!(
        state.index_message.lock().expect("lock").clone(),
        Some("indexing".to_string())
    );
    let end = json!({"params": {"token": "t1", "value": {"kind": "end"}}});
    track_progress(&state, &end);
    assert!(state.progress.lock().expect("lock").is_empty());
    assert!(state.index_message.lock().expect("lock").is_none());
}

/// A request resolves with the peer's result, and a structured error
/// surfaces with its code.
#[tokio::test]
async fn test_request_answers() {
    let (client, _peer) = pair(|frame| {
        let id = frame.get("id").cloned();
        match frame.get("method").and_then(Value::as_str) {
            Some("textDocument/hover") => {
                vec![json!({"jsonrpc": "2.0", "id": id, "result": {"contents": "hi"}})]
            }
            _ => vec![json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32602, "message": "bad params"}})],
        }
    });
    let answer = client
        .request("textDocument/hover", json!({}))
        .await
        .expect("answers");
    assert_eq!(answer["contents"], "hi");
    let error = client
        .request("textDocument/bad", json!({}))
        .await
        .expect_err("refused");
    assert_eq!(error.code, -32602);
    assert!(error.to_string().contains("bad params"));
}

/// The reverse requests a server sends are answered with the shapes the
/// protocol expects, and an unknown request is refused, not ignored.
#[tokio::test]
async fn test_reverse_requests_answered() {
    let (client, peer) = pair(|frame| {
        let id = frame.get("id").cloned();
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                vec![json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}})]
            }
            _ => vec![],
        }
    });
    client
        .initialize(json!({}), Duration::from_secs(2))
        .await
        .expect("handshake");
    peer.out
        .send(
            json!({"jsonrpc": "2.0", "id": 100, "method": "workspace/configuration",
            "params": {"items": [{}, {}]}}),
        )
        .expect("sent");
    peer.out
        .send(
            json!({"jsonrpc": "2.0", "id": 101, "method": "window/workDoneProgress/create",
            "params": {"token": "t"}}),
        )
        .expect("sent");
    peer.out
        .send(json!({"jsonrpc": "2.0", "id": 102, "method": "custom/unknown"}))
        .expect("sent");
    let started = std::time::Instant::now();
    loop {
        let replies = {
            let seen = peer.seen.lock().expect("seen");
            seen.iter()
                .filter(|value| {
                    value
                        .get("id")
                        .and_then(Value::as_u64)
                        .is_some_and(|id| id >= 100)
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        if replies.len() >= 3 {
            let configuration = replies
                .iter()
                .find(|value| value.get("id") == Some(&json!(100)))
                .expect("configuration answered");
            assert_eq!(configuration["result"], json!([null, null]));
            let progress = replies
                .iter()
                .find(|value| value.get("id") == Some(&json!(101)))
                .expect("progress answered");
            assert_eq!(progress["result"], Value::Null);
            let unknown = replies
                .iter()
                .find(|value| value.get("id") == Some(&json!(102)))
                .expect("unknown answered");
            assert_eq!(unknown["error"]["code"], -32601);
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the peer answered every request"
        );
        sleep(Duration::from_millis(2)).await;
    }
}

/// A notification the server publishes reaches the owner's stream.
#[tokio::test]
async fn test_notification_forwarded() {
    let (client_read, server_write) = duplex(64 * 1024);
    let (server_read, client_write) = duplex(64 * 1024);
    let peer = spawn_peer(server_read, server_write, |_| vec![]);
    let (_client, mut notices) = LspClient::spawn(client_read, client_write, DEFAULT_RETRY_DELAYS);
    peer.out
        .send(
            json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///a.rs", "diagnostics": []}}),
        )
        .expect("sent");
    let notice = timeout(Duration::from_secs(5), notices.recv())
        .await
        .expect("a notice arrives")
        .expect("the stream is open");
    assert_eq!(notice.method, "textDocument/publishDiagnostics");
    assert_eq!(notice.params["uri"], "file:///a.rs");
}

/// A request answered with the transient state-changed error is retried and
/// then succeeds.
#[tokio::test]
async fn test_transient_error_retries() {
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&attempts);
    let (client, _peer) = pair(move |frame| {
        let id = frame.get("id").cloned();
        let count = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if count < 2 {
            vec![json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32801, "message": "content modified"}})]
        } else {
            vec![json!({"jsonrpc": "2.0", "id": id, "result": "settled"})]
        }
    });
    let answer = client
        .request("textDocument/hover", json!({}))
        .await
        .expect("settles");
    assert_eq!(answer, "settled");
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
}

/// The graceful stop sends shutdown then exit, and the stream end settles a
/// waiter with the closed error.
#[tokio::test]
async fn test_shutdown_and_closed() {
    let (client, peer) = pair(|frame| {
        let id = frame.get("id").cloned();
        match frame.get("method").and_then(Value::as_str) {
            Some("shutdown") => vec![json!({"jsonrpc": "2.0", "id": id, "result": null})],
            _ => vec![],
        }
    });
    client.shutdown(Duration::from_secs(2)).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let methods: Vec<String> = peer
            .seen
            .lock()
            .expect("seen")
            .iter()
            .filter_map(|value| {
                value
                    .get("method")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        if methods.contains(&"shutdown".to_string()) && methods.contains(&"exit".to_string()) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "both frames reached the peer, saw {methods:?}"
        );
        sleep(Duration::from_millis(2)).await;
    }
    drop(peer);
    client.wait_closed().await;
    assert!(client.is_closed());
}

/// A reply that arrives for an abandoned retry attempt is ignored: ids are
/// never reused, so a late duplicate finds no waiter and changes nothing.
#[tokio::test]
async fn test_stale_reply_ignored() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = std::sync::Arc::clone(&seen);
    let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let first_flag = std::sync::Arc::clone(&first);
    let (client, peer) = pair(move |frame| {
        let id = frame.get("id").cloned();
        if let Ok(mut log) = log.lock() {
            log.push(frame.clone());
        }
        if first_flag.swap(false, std::sync::atomic::Ordering::SeqCst) {
            vec![json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32801, "message": "content modified"}})]
        } else {
            vec![json!({"jsonrpc": "2.0", "id": id, "result": "fresh"})]
        }
    });
    let answer = client
        .request("textDocument/hover", json!({}))
        .await
        .expect("settles");
    assert_eq!(answer, "fresh");
    let abandoned = peer
        .seen
        .lock()
        .expect("seen")
        .first()
        .and_then(|frame| frame.get("id").cloned())
        .expect("the first id");
    peer.out
        .send(json!({"jsonrpc": "2.0", "id": abandoned, "result": "stale"}))
        .expect("sent");
    let next = client
        .request("textDocument/hover", json!({}))
        .await
        .expect("answers");
    assert_eq!(
        next, "fresh",
        "the stray reply did not disturb later requests"
    );
}
