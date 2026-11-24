//! A socket-backed stub tool server for the MCP integration tests.
//!
//! The launcher's pipes are the client end of a loopback socket, so the whole
//! path under test — spawn, bridge, transport, handshake — runs exactly as it
//! does for a real child, while the server side is a thread speaking the
//! protocol from a table.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use houyicoder_api::launcher::{
    LauncherChild, LauncherExit, ProcessLauncher, SpawnError, SpawnPolicy, SpawnRequest, StdioPipes,
};

/// What one fake server does once the client confirms the handshake.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// Emit a burst of change notices.
    Burst,
    /// Close the transport immediately.
    CloseNow,
    /// Stay connected and answer listings.
    Calm,
}

/// What the manager launched and what the servers were asked for.
#[derive(Default)]
pub struct Counters {
    pub spawns: AtomicUsize,
    pub tool_lists: AtomicUsize,
    pub tool_calls: AtomicUsize,
}

/// A launcher whose child speaks the protocol on a loopback socket.
pub struct SocketLauncher {
    pub counters: Arc<Counters>,
    pub behaviors: Arc<Mutex<Vec<Behavior>>>,
    /// The text a tools/call request is answered with.
    pub call_text: Arc<Mutex<String>>,
}

impl SocketLauncher {
    /// A launcher over the given behaviors, answering calls with one text.
    pub fn new(behaviors: Vec<Behavior>) -> Self {
        Self {
            counters: Arc::new(Counters::default()),
            behaviors: Arc::new(Mutex::new(behaviors)),
            call_text: Arc::new(Mutex::new("answer".to_string())),
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
        let call_text = Arc::clone(&self.call_text);
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                serve_fake(stream, counters, call_text, behavior);
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

/// The fake server: answers initialize, tools/list, and tools/call, counts
/// what it was asked, and runs the behavior once the handshake is confirmed.
fn serve_fake(
    mut stream: TcpStream,
    counters: Arc<Counters>,
    call_text: Arc<Mutex<String>>,
    behavior: Behavior,
) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let id = frame.get("id").cloned();
        let method = frame.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let reply = match method {
            "initialize" => Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": "fake", "version": "1.0"},
                    "instructions": "use probe"
                }
            })),
            "tools/list" => {
                counters.tool_lists.fetch_add(1, Ordering::SeqCst);
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"tools": [
                        {
                            "name": "probe",
                            "description": "a probe",
                            "inputSchema": {"type": "object"},
                            "annotations": {"readOnlyHint": true}
                        },
                        {
                            "name": "wipe",
                            "description": "wipes everything",
                            "inputSchema": {"type": "object"}
                        }
                    ]}
                }))
            }
            "tools/call" => {
                counters.tool_calls.fetch_add(1, Ordering::SeqCst);
                let text = call_text
                    .lock()
                    .map(|text| text.clone())
                    .unwrap_or_default();
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"content": [{"type": "text", "text": text}]}
                }))
            }
            "notifications/initialized" => {
                match behavior {
                    Behavior::Burst => {
                        let mut frames = String::new();
                        for _ in 0..100 {
                            frames.push_str(
                                "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n",
                            );
                        }
                        if stream.write_all(frames.as_bytes()).is_err() {
                            return;
                        }
                        if stream.flush().is_err() {
                            return;
                        }
                    }
                    Behavior::CloseNow => return,
                    Behavior::Calm => {}
                }
                None
            }
            _ => None,
        };
        if let Some(reply) = reply {
            let mut text = serde_json::to_string(&reply).expect("serializes");
            text.push('\n');
            if stream.write_all(text.as_bytes()).is_err() || stream.flush().is_err() {
                return;
            }
        }
    }
}
