//! A socket-backed stub language server for the LSP integration tests.
//!
//! The launcher's pipes are the client end of a loopback socket, so the
//! whole path under test, launch through handshake through framed requests,
//! runs as it does for a real child, while the server side is a thread
//! speaking framed JSON-RPC from a table.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::Duration;

use houyicoder_api::launcher::{
    LauncherChild, LauncherExit, ProcessLauncher, SpawnError, SpawnPolicy, SpawnRequest, StdioPipes,
};
use serde_json::Value;
use serde_json::json;

/// What one fake server does beyond answering the handshake.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// Answer everything normally and stay connected.
    Calm,
    /// Close the transport right after the handshake confirms.
    ExitAfterInit,
    /// Answer the first three queries with the transient state-changed
    /// error, then answer normally.
    ModifiedThenOk,
    /// Open one indexing progress token after the handshake and close it a
    /// moment later.
    ProgressThenEnd,
    /// Delay the handshake answer, so a test can act while a launch is in
    /// flight.
    SlowInit,
}

/// What the launcher started and what the servers were asked.
#[derive(Default)]
pub struct Counters {
    pub spawns: AtomicUsize,
    pub initializes: AtomicUsize,
    /// Connections currently served.
    pub live: AtomicUsize,
    pub calls: Mutex<BTreeMap<String, usize>>,
    /// The document notifications received, in order.
    pub documents: Mutex<Vec<(String, Value)>>,
}

impl Counters {
    /// How many times one method was requested.
    pub fn calls_of(&self, method: &str) -> usize {
        self.calls
            .lock()
            .map(|calls| calls.get(method).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    /// The versions the server received on document changes, in order.
    pub fn change_versions(&self) -> Vec<i64> {
        self.documents
            .lock()
            .map(|documents| {
                documents
                    .iter()
                    .filter(|(method, _)| method == "textDocument/didChange")
                    .filter_map(|(_, params)| {
                        params
                            .get("textDocument")
                            .and_then(|doc| doc.get("version"))
                            .and_then(Value::as_i64)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A launcher whose child speaks the protocol on a loopback socket.
pub struct SocketLauncher {
    pub counters: Arc<Counters>,
    behaviors: Arc<Mutex<Vec<Behavior>>>,
}

impl SocketLauncher {
    /// A launcher over the given behaviors, answering calmly once they run
    /// out.
    pub fn new(behaviors: Vec<Behavior>) -> Self {
        Self {
            counters: Arc::new(Counters::default()),
            behaviors: Arc::new(Mutex::new(behaviors)),
        }
    }
}

impl ProcessLauncher for SocketLauncher {
    fn spawn(&self, _req: SpawnRequest, _policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        self.counters.spawns.fetch_add(1, Ordering::SeqCst);
        let behavior = self
            .behaviors
            .lock()
            .map(|mut queue| {
                if queue.is_empty() {
                    Behavior::Calm
                } else {
                    queue.remove(0)
                }
            })
            .unwrap_or(Behavior::Calm);
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|error| SpawnError::Io(error.to_string()))?;
        let address = listener
            .local_addr()
            .map_err(|error| SpawnError::Io(error.to_string()))?;
        let counters = Arc::clone(&self.counters);
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                counters.live.fetch_add(1, Ordering::SeqCst);
                serve_fake(stream, Arc::clone(&counters), behavior);
                counters.live.fetch_sub(1, Ordering::SeqCst);
            }
        });
        let client =
            TcpStream::connect(address).map_err(|error| SpawnError::Io(error.to_string()))?;
        let writer = client
            .try_clone()
            .map_err(|error| SpawnError::Io(error.to_string()))?;
        let pipes = StdioPipes {
            stdin: Some(Box::new(writer)),
            stdout: Some(Box::new(client)),
            stderr: None,
        };
        let waiter: houyicoder_async::PFut<'static, Result<LauncherExit, SpawnError>> =
            Box::pin(async {
                Ok(LauncherExit {
                    exit_code: Some(0),
                    stdout: None,
                    stderr: None,
                })
            });
        Ok(LauncherChild::with_pipes(Some(0), pipes, waiter))
    }
}

/// The fake server: framed JSON-RPC over one socket, scripted by behavior.
fn serve_fake(stream: TcpStream, counters: Arc<Counters>, behavior: Behavior) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let out = Arc::new(Mutex::new(stream));
    let mut reader = BufReader::new(read_half);
    let mut modified_left = 3usize;
    loop {
        let Ok(Some(body)) = read_message(&mut reader) else {
            return;
        };
        let Ok(frame) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        let id = frame.get("id").cloned();
        let method = frame
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Ok(mut calls) = counters.calls.lock() {
            *calls.entry(method.clone()).or_insert(0) += 1;
        }
        match method.as_str() {
            "initialize" => {
                counters.initializes.fetch_add(1, Ordering::SeqCst);
                if behavior == Behavior::SlowInit {
                    sleep(Duration::from_millis(150));
                }
                reply(
                    &out,
                    id,
                    json!({
                        "capabilities": {"textDocumentSync": {"didSave": true}},
                        "serverInfo": {"name": "fake", "version": "1.0"},
                    }),
                );
            }
            "initialized" => {
                if behavior == Behavior::ExitAfterInit {
                    return;
                }
                if behavior == Behavior::ProgressThenEnd {
                    notify(
                        &out,
                        "$/progress",
                        json!({
                            "token": "index", "value": {"kind": "begin", "message": "indexing"}
                        }),
                    );
                    let closer = Arc::clone(&out);
                    std::thread::spawn(move || {
                        sleep(Duration::from_millis(60));
                        notify(
                            &closer,
                            "$/progress",
                            json!({
                                "token": "index", "value": {"kind": "end"}
                            }),
                        );
                    });
                }
            }
            "textDocument/didOpen" | "textDocument/didChange" | "textDocument/didSave" => {
                let params = frame.get("params").cloned().unwrap_or(Value::Null);
                if let Ok(mut documents) = counters.documents.lock() {
                    documents.push((method, params));
                }
            }
            "textDocument/hover" => {
                if behavior == Behavior::ModifiedThenOk && modified_left > 0 {
                    modified_left -= 1;
                    error_reply(&out, id, -32801, "content modified");
                } else {
                    reply(
                        &out,
                        id,
                        json!({
                            "contents": {"kind": "plaintext", "value": "hover answer"}
                        }),
                    );
                }
            }
            "shutdown" => reply(&out, id, Value::Null),
            "exit" => return,
            _ => {
                if let Some(id) = id {
                    error_reply(&out, Some(id), -32601, "no such method");
                }
            }
        }
    }
}

/// Read one framed message body.
fn read_message(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<Vec<u8>>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let Some(length) = length else {
        return Ok(None);
    };
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Write one framed reply for a request.
fn reply(out: &Arc<Mutex<TcpStream>>, id: Option<Value>, result: Value) {
    send(out, json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

/// Write one framed error for a request.
fn error_reply(out: &Arc<Mutex<TcpStream>>, id: Option<Value>, code: i64, message: &str) {
    send(
        out,
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    );
}

/// Write one framed notification.
fn notify(out: &Arc<Mutex<TcpStream>>, method: &str, params: Value) {
    send(
        out,
        json!({"jsonrpc": "2.0", "method": method, "params": params}),
    );
}

/// Frame and write one message.
fn send(out: &Arc<Mutex<TcpStream>>, value: Value) {
    let Ok(body) = serde_json::to_vec(&value) else {
        return;
    };
    let Ok(mut stream) = out.lock() else {
        return;
    };
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    drop(stream.write_all(header.as_bytes()));
    drop(stream.write_all(&body));
    drop(stream.flush());
}
