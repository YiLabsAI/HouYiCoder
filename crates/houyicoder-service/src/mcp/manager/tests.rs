//! Tests for this module.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::time::timeout;

use houyicoder_api::launcher::{
    LauncherChild, LauncherExit, ProcessLauncher, SpawnError, SpawnPolicy, SpawnRequest, StdioPipes,
};
use houyicoder_protocol::mcp::McpConfigScope;

use super::*;

/// What one fake server does once the client confirms the handshake.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Behavior {
    /// Emit a burst of change notices.
    Burst,
    /// Close the transport immediately.
    CloseNow,
    /// Stay connected and answer listings.
    Calm,
}

/// Counts what the manager launched and what the servers were asked for.
#[derive(Default)]
struct Counters {
    spawns: AtomicUsize,
    tool_lists: AtomicUsize,
}

/// A launcher whose child speaks the protocol on a loopback socket. The pipe
/// pair the bridge receives is the client end of that socket, so the whole
/// spawn-to-transport path runs exactly as it does for a real child.
struct SocketLauncher {
    counters: Arc<Counters>,
    behaviors: Arc<Mutex<Vec<Behavior>>>,
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
                serve_fake(stream, counters, behavior);
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

/// The fake server: answers initialize and tools/list, counts listings, and
/// runs the behavior once the handshake is confirmed.
fn serve_fake(mut stream: TcpStream, counters: Arc<Counters>, behavior: Behavior) {
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
                    "result": {"tools": [{
                        "name": "probe",
                        "description": "a probe",
                        "inputSchema": {"type": "object"}
                    }]}
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

fn manager_with(behaviors: Vec<Behavior>, timings: McpTimings) -> (McpConnections, Arc<Counters>) {
    let counters = Arc::new(Counters::default());
    let launcher = Arc::new(SocketLauncher {
        counters: Arc::clone(&counters),
        behaviors: Arc::new(Mutex::new(behaviors)),
    });
    let manager = McpConnections::new(launcher, Arc::from(std::path::Path::new("/work")), timings);
    (manager, counters)
}

fn config(name: &str, disabled: bool) -> ScopedMcpServerConfig {
    ScopedMcpServerConfig {
        name: name.into(),
        scope: McpConfigScope::Project,
        transport: houyicoder_protocol::mcp::McpTransportConfig::stdio("fake", vec![]),
        disabled,
        tool_timeout_ms: 5_000,
    }
}

fn fast_timings() -> McpTimings {
    McpTimings {
        notice_window: Duration::from_millis(30),
        handshake_timeout: Duration::from_millis(500),
        liveness_poll: Duration::from_millis(5),
        stable_after: Duration::from_millis(20),
        restart: RestartPolicy::new(Duration::from_millis(1), Duration::from_millis(2), 2),
    }
}

/// Poll until the predicate holds or the deadline passes.
async fn eventually(mut predicate: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if predicate() {
            return true;
        }
        sleep(Duration::from_millis(2)).await;
    }
    predicate()
}

/// An untrusted workspace's servers are disabled before the manager sees
/// them, and a disabled entry never reaches the launcher: the spawn count
/// stays zero for both child-process and endpoint entries.
#[tokio::test]
async fn test_untrusted_never_spawns() {
    let (manager, counters) = manager_with(Vec::new(), fast_timings());
    let mut gated = vec![config("spawny", false), config("remote", false)];
    gated[1].transport =
        houyicoder_protocol::mcp::McpTransportConfig::http("https://example.invalid/mcp");
    let gated = houyicoder_config::apply_trust_gate(gated, false);
    manager.start_all(gated).await;
    let statuses = manager.statuses().await;
    assert_eq!(statuses.len(), 2);
    for status in &statuses {
        assert!(
            matches!(status.state, McpConnectionState::Disabled { .. }),
            "{} is disabled: {:?}",
            status.name,
            status.state
        );
    }
    assert_eq!(
        counters.spawns.load(Ordering::SeqCst),
        0,
        "no process is started for an untrusted workspace"
    );
    manager.shutdown().await;
}

/// A burst of change notices inside the window folds into one refetch: the
/// server sees the initial listing plus exactly one more.
#[tokio::test]
async fn test_list_changed_coalesced() {
    let (manager, counters) = manager_with(vec![Behavior::Burst], fast_timings());
    manager.start_all(vec![config("chatty", false)]).await;
    assert!(
        eventually(|| counters.tool_lists.load(Ordering::SeqCst) == 2).await,
        "the burst produced one refetch, saw {} listings",
        counters.tool_lists.load(Ordering::SeqCst)
    );
    // Give a second window room to open: no further refetch may follow.
    sleep(Duration::from_millis(80)).await;
    assert_eq!(
        counters.tool_lists.load(Ordering::SeqCst),
        2,
        "one window, one refetch"
    );
    manager.shutdown().await;
}

/// A connection that keeps dropping is restarted the policy's number of
/// times and then reported failed, not restarted forever.
#[tokio::test]
async fn test_restart_bounded() {
    let behaviors = vec![Behavior::CloseNow, Behavior::CloseNow, Behavior::CloseNow];
    let (manager, counters) = manager_with(behaviors, fast_timings());
    manager.start_all(vec![config("flaky", false)]).await;
    let failed = Arc::new(std::sync::Mutex::new(false));
    let probe_manager = manager.clone();
    let probe_failed = Arc::clone(&failed);
    let watcher = tokio::spawn(async move {
        loop {
            let statuses = probe_manager.statuses().await;
            if statuses
                .iter()
                .any(|status| matches!(status.state, McpConnectionState::Failed { .. }))
            {
                *probe_failed.lock().expect("lock") = true;
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
    });
    let settled = timeout(Duration::from_secs(5), watcher).await;
    assert!(settled.is_ok(), "the server reaches a failed state");
    assert!(*failed.lock().expect("lock"));
    assert_eq!(
        counters.spawns.load(Ordering::SeqCst),
        3,
        "the policy's two restarts plus the first launch"
    );
    manager.shutdown().await;
}

/// A reconnect request launches the server again after it failed, and a
/// graceful shutdown ends every task.
#[tokio::test]
async fn test_reconnect_after_failure() {
    let behaviors = vec![Behavior::CloseNow, Behavior::CloseNow, Behavior::CloseNow];
    let (manager, counters) = manager_with(behaviors, fast_timings());
    manager.start_all(vec![config("flaky", false)]).await;
    assert!(
        eventually(|| counters.spawns.load(Ordering::SeqCst) == 3).await,
        "the retries ran"
    );
    assert!(
        eventually_status(&manager, |state| {
            matches!(state, McpConnectionState::Failed { .. })
        })
        .await,
        "the server is failed before the reconnect"
    );
    // A calm launch on the next attempt, then the user asks for a retry.
    manager.reconnect("flaky").await;
    assert!(
        eventually(|| counters.spawns.load(Ordering::SeqCst) == 4).await,
        "the reconnect started the server again"
    );
    manager.shutdown().await;
}

/// The tool snapshot is ordered by server name then tool name, so the list
/// the model sees does not depend on handshake order.
#[tokio::test]
async fn test_tools_ordered() {
    let (manager, _counters) = manager_with(Vec::new(), fast_timings());
    manager
        .start_all(vec![config("beta", false), config("alpha", false)])
        .await;
    assert!(
        eventually_status(&manager, |state| {
            matches!(state, McpConnectionState::Connected)
        })
        .await,
        "the first server connects"
    );
    // Both servers connect to their own fake; wait for both.
    let started = Instant::now();
    loop {
        let tools = manager.tools_snapshot().await;
        if tools.len() == 2 {
            assert_eq!(tools[0].0, "alpha");
            assert_eq!(tools[1].0, "beta");
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "both servers connect"
        );
        sleep(Duration::from_millis(5)).await;
    }
    manager.shutdown().await;
}

async fn eventually_status(
    manager: &McpConnections,
    predicate: impl Fn(&McpConnectionState) -> bool,
) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        let statuses = manager.statuses().await;
        if statuses.iter().any(|status| predicate(&status.state)) {
            return true;
        }
        sleep(Duration::from_millis(5)).await;
    }
    false
}

/// The production defaults are what the manager claims: a short notice
/// window, a bounded handshake, the retry policy, and the diagnostics hold.
#[test]
fn test_defaults_hold() {
    let timings = McpTimings::default();
    assert_eq!(timings.notice_window, Duration::from_millis(50));
    assert_eq!(timings.handshake_timeout, Duration::from_secs(15));
    assert_eq!(timings.liveness_poll, Duration::from_millis(500));
    assert_eq!(timings.stable_after, Duration::from_secs(30));
    assert_eq!(timings.restart.delay(1), Some(Duration::from_secs(1)));
    assert_eq!(timings.restart.delay(4), None, "three retries by default");
}

/// The diagnostics accessors answer for a server that wrote nothing and for
/// an unknown name, so a status surface renders an empty tail, not an error.
#[tokio::test]
async fn test_stderr_accessors() {
    let (manager, _counters) = manager_with(Vec::new(), fast_timings());
    manager.start_all(vec![config("quiet", false)]).await;
    assert!(manager.stderr_tail("quiet").await.is_empty());
    assert!(manager.stderr_tail("nobody").await.is_empty());
    assert_eq!(manager.stderr_limit(), STDERR_TAIL_LINES);
    manager.shutdown().await;
}
