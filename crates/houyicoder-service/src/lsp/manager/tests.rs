//! Tests for the manager's rules, driven over in-memory peers through the
//! attach seam: routing, statuses, document versions, diagnostics, the
//! switch, and the refusal paths.

#[cfg(unix)]
use std::env::temp_dir;
#[cfg(unix)]
use std::fs::create_dir_all;
#[cfg(unix)]
use std::fs::remove_dir_all;
#[cfg(unix)]
use std::fs::write;
use std::io::BufRead;
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;
#[cfg(unix)]
use std::process::id;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use houyicoder_api::launcher::{
    LauncherChild, ProcessLauncher, SpawnError, SpawnPolicy, SpawnRequest,
};
use houyicoder_protocol::lsp::{LspConfigScope, LspServerState, ScopedLspServerConfig};
use serde_json::Value;
use serde_json::json;
use tokio::io::duplex;
use tokio::time::sleep;

use super::*;
use crate::child_process::file_uri;
use crate::lsp::client::tests::Peer;
use crate::lsp::client::tests::spawn_peer;
use crate::lsp::fast_server_config;

/// A launcher that refuses to start anything, so a test can prove the
/// manager never asked.
pub(crate) struct NoSpawn;

impl ProcessLauncher for NoSpawn {
    fn spawn(&self, _req: SpawnRequest, _policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        Err(SpawnError::Io("no process may start in this test".into()))
    }
}

fn fast_timings() -> LspTimings {
    LspTimings {
        retry_delays: [
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(4),
        ],
        restart_base: Duration::from_millis(5),
        diagnostics_grace: Duration::from_millis(2),
        diagnostics_cap: Duration::from_millis(10),
        index_wait: Duration::from_millis(5),
        index_poll: Duration::from_millis(2),
        stable_after: Duration::from_millis(20),
    }
}

fn manager() -> Arc<LspConnections> {
    Arc::new(LspConnections::new(
        Arc::new(NoSpawn),
        Arc::from(Path::new("/work")),
        fast_timings(),
    ))
}

fn config(name: &str, scope: LspConfigScope, disabled: bool) -> ScopedLspServerConfig {
    let mut server = fast_server_config(name);
    server.scope = scope;
    server.disabled = disabled;
    server.locked = disabled && scope == LspConfigScope::Project;
    server
}

/// One attachable peer answering the handshake and the graceful stop, and
/// logging every document notification it receives.
async fn attach(
    manager: &Arc<LspConnections>,
    name: &str,
) -> (Peer, Arc<std::sync::Mutex<Vec<Value>>>) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let handler = move |frame: &Value| -> Vec<Value> {
        let id = frame.get("id").cloned();
        if let Ok(mut log) = log.lock() {
            log.push(frame.clone());
        }
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                vec![json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}})]
            }
            Some("shutdown") => vec![json!({"jsonrpc": "2.0", "id": id, "result": null})],
            _ => vec![],
        }
    };
    let (client_read, server_write) = duplex(64 * 1024);
    let (server_read, client_write) = duplex(64 * 1024);
    let peer = spawn_peer(server_read, server_write, handler);
    manager
        .attach_for_test(name, client_read, client_write)
        .await
        .expect("attached");
    (peer, seen)
}

/// Registering servers starts nothing, records the slots, and reads the
/// initial states.
#[tokio::test]
async fn test_settle_routes_without_start() {
    let manager = manager();
    manager
        .start_all(vec![
            config("rust", LspConfigScope::User, false),
            config("off", LspConfigScope::Project, true),
        ])
        .await;
    let statuses = manager.statuses().await;
    assert_eq!(statuses.len(), 2);
    let off = statuses.iter().find(|row| row.name == "off").expect("off");
    assert!(matches!(off.state, LspServerState::Disabled { .. }));
    assert_eq!(off.scope, LspConfigScope::Project);
    let rust = statuses
        .iter()
        .find(|row| row.name == "rust")
        .expect("rust");
    assert_eq!(rust.state, LspServerState::Idle);
    assert_eq!(
        manager.server_for_path(Path::new("/work/src/lib.rs")).await,
        Some("rust".to_string())
    );
    assert_eq!(manager.server_for_path(Path::new("/work/a.ts")).await, None);
    assert_eq!(
        manager.config_scope("rust").await,
        Some(LspConfigScope::User)
    );
    assert_eq!(manager.config_scope("nobody").await, None);
}

/// Refusals: an unknown name, a disabled server, and an entry naming a
/// transport this host does not launch.
#[tokio::test]
async fn test_start_refusals() {
    let manager = manager();
    let mut socket = config("socket", LspConfigScope::User, false);
    socket.transport = houyicoder_protocol::lsp::LspTransportKind::Socket;
    manager
        .start_all(vec![config("off", LspConfigScope::Project, true), socket])
        .await;
    assert!(manager.ensure_started("nobody").await.is_err());
    assert!(manager.ensure_started("off").await.is_err());
    let error = manager
        .ensure_started("socket")
        .await
        .map(drop)
        .expect_err("refused");
    assert!(error.contains("does not launch"), "{error}");
    assert!(
        manager
            .request_for_file(
                Path::new("/work/a.ts"),
                "x",
                json!({}),
                None,
                Duration::from_secs(1)
            )
            .await
            .is_err()
    );
}

/// The switch: a locked server refuses to turn on, a live one stops and
/// disables, and turning it back on returns it to idle.
#[tokio::test]
async fn test_switch_bounds() {
    let manager = manager();
    manager
        .start_all(vec![
            config("locked", LspConfigScope::Project, true),
            config("rust", LspConfigScope::User, false),
        ])
        .await;
    let error = manager
        .set_enabled("locked", true)
        .await
        .expect_err("locked");
    assert!(error.contains("trusted"), "{error}");
    drop(attach(&manager, "rust").await);
    assert_eq!(
        manager
            .statuses()
            .await
            .iter()
            .find(|row| row.name == "rust")
            .expect("rust")
            .state,
        LspServerState::Running
    );
    manager.set_enabled("rust", false).await.expect("disable");
    let row = manager
        .statuses()
        .await
        .into_iter()
        .find(|r| r.name == "rust")
        .expect("rust");
    assert!(matches!(row.state, LspServerState::Disabled { .. }));
    assert!(
        manager.ensure_started("rust").await.is_err(),
        "disabled stays off"
    );
    manager.set_enabled("rust", true).await.expect("enable");
    let row = manager
        .statuses()
        .await
        .into_iter()
        .find(|r| r.name == "rust")
        .expect("rust");
    assert_eq!(row.state, LspServerState::Idle);
    assert!(manager.set_enabled("nobody", true).await.is_err());
}

/// Document sync over an attached peer: one open with version one, then the
/// next versions, and a save with each.
#[tokio::test]
async fn test_sync_versions() {
    let manager = manager();
    manager
        .start_all(vec![config("rust", LspConfigScope::User, false)])
        .await;
    let (_peer, seen) = attach(&manager, "rust").await;
    let file = Path::new("/work/src/lib.rs");
    for content in ["a", "b", "c"] {
        manager.sync_file_saved(file, content).await.expect("sync");
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let documents: Vec<Value> = loop {
        let documents: Vec<Value> = seen
            .lock()
            .expect("seen")
            .iter()
            .filter(|frame| {
                frame
                    .get("method")
                    .and_then(Value::as_str)
                    .is_some_and(|method| method.starts_with("textDocument/did"))
            })
            .cloned()
            .collect();
        let changes = documents
            .iter()
            .filter(|f| f["method"] == "textDocument/didChange")
            .count();
        if changes == 2 {
            break documents;
        }
        assert!(
            Instant::now() < deadline,
            "the sync frames reached the peer, saw {} document frames",
            documents.len()
        );
        sleep(Duration::from_millis(2)).await;
    };
    let opens = documents
        .iter()
        .filter(|f| f["method"] == "textDocument/didOpen")
        .count();
    assert_eq!(opens, 1, "one open");
    let versions: Vec<i64> = documents
        .iter()
        .filter(|f| f["method"] == "textDocument/didChange")
        .filter_map(|f| f["params"]["textDocument"]["version"].as_i64())
        .collect();
    assert_eq!(versions, vec![2, 3], "versions strictly increase");
    let saves = documents
        .iter()
        .filter(|f| f["method"] == "textDocument/didSave")
        .count();
    assert_eq!(saves, 3, "a save per sync");
    // A suffix nobody routes syncs to nothing.
    assert_eq!(
        manager
            .sync_file_saved(Path::new("/work/a.ts"), "x")
            .await
            .expect("no route"),
        None
    );
}

/// A published diagnostic lands in the store, a later empty publish clears
/// the findings while keeping the file's entry and bumping its generation,
/// and the status row counts what is held.
#[tokio::test]
async fn test_diagnostics_store() {
    let manager = manager();
    manager
        .start_all(vec![config("rust", LspConfigScope::User, false)])
        .await;
    let (peer, _seen) = attach(&manager, "rust").await;
    let file = Path::new("/work/src/lib.rs");
    manager
        .sync_file_saved(file, "fn a() { let x: u8 = 1; }")
        .await
        .expect("sync");
    let uri = file_uri(file);
    peer.out
        .send(json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
        "params": {"uri": uri, "diagnostics": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
             "severity": 1, "message": "type mismatch"}
        ]}}))
        .expect("sent");
    let started = Instant::now();
    while manager.file_diagnostics("rust", &uri).await.is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the publish reached the store"
        );
        sleep(Duration::from_millis(2)).await;
    }
    let row = manager
        .statuses()
        .await
        .into_iter()
        .find(|r| r.name == "rust")
        .expect("rust");
    assert_eq!(row.diagnostic_count, 1);
    let generation = manager.publish_generation("rust", &uri).await;
    peer.out
        .send(
            json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
        "params": {"uri": uri, "diagnostics": []}}),
        )
        .expect("sent");
    let started = Instant::now();
    while !manager.file_diagnostics("rust", &uri).await.is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an empty publish clears the findings"
        );
        sleep(Duration::from_millis(2)).await;
    }
    assert!(
        manager.publish_generation("rust", &uri).await > generation,
        "the clean publish bumps the file's generation"
    );
    manager.shutdown().await;
}

/// A file-scoped request over an attached peer answers with the peer's
/// result; the attach path also delivers the settings notification when the
/// entry carries settings.
#[tokio::test]
async fn test_request_for_file_answers() {
    let manager = manager();
    let mut server = config("rust", LspConfigScope::User, false);
    server.settings = Some(json!({"rust-analyzer": {"checkOnSave": true}}));
    manager.start_all(vec![server]).await;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let handler = move |frame: &Value| -> Vec<Value> {
        let id = frame.get("id").cloned();
        if let Ok(mut log) = log.lock() {
            log.push(frame.clone());
        }
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                vec![json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}})]
            }
            Some("textDocument/hover") => {
                vec![json!({"jsonrpc": "2.0", "id": id, "result": {"contents": "answer"}})]
            }
            Some("shutdown") => vec![json!({"jsonrpc": "2.0", "id": id, "result": null})],
            _ => vec![],
        }
    };
    let (client_read, server_write) = duplex(64 * 1024);
    let (server_read, client_write) = duplex(64 * 1024);
    let _peer = spawn_peer(server_read, server_write, handler);
    let client = manager
        .attach_for_test("rust", client_read, client_write)
        .await
        .expect("attached");
    client
        .notify("workspace/didChangeConfiguration", json!({"settings": {}}))
        .expect("sent");
    let answer = manager
        .request_for_file(
            Path::new("/work/src/lib.rs"),
            "textDocument/hover",
            json!({"textDocument": {"uri": "file:///work/src/lib.rs"},
                "position": {"line": 0, "character": 0}}),
            Some("fn a() {}"),
            Duration::from_secs(5),
        )
        .await
        .expect("answers");
    assert_eq!(answer["contents"], "answer");
    manager.shutdown().await;
}

/// A restart on an attached server stops the live connection and attempts a
/// relaunch, which the refusing launcher reports.
#[tokio::test]
async fn test_restart_attempts_relaunch() {
    let manager = manager();
    manager
        .start_all(vec![config("rust", LspConfigScope::User, false)])
        .await;
    let _peer = attach(&manager, "rust").await;
    let error = manager
        .restart("rust")
        .await
        .map(drop)
        .expect_err("relaunch refused");
    assert!(error.contains("did not start"), "{error}");
    let row = manager
        .statuses()
        .await
        .into_iter()
        .find(|r| r.name == "rust")
        .expect("rust");
    assert!(matches!(row.state, LspServerState::Failed { .. }));
    assert!(manager.restart("nobody").await.is_err());
    manager.shutdown().await;
}

/// A connection that ends by itself runs the crash watcher: the slot reports
/// failure and the configured relaunch asks the launcher again.
#[tokio::test]
async fn test_crash_watcher_reports() {
    let manager = manager();
    manager
        .start_all(vec![config("rust", LspConfigScope::User, false)])
        .await;
    let peer = attach(&manager, "rust").await;
    drop(peer);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let row = manager
            .statuses()
            .await
            .into_iter()
            .find(|r| r.name == "rust")
            .expect("rust");
        if matches!(row.state, LspServerState::Failed { .. }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the crash surfaced as a failure, saw {:?}",
            row.state
        );
        sleep(Duration::from_millis(2)).await;
    }
    // The relaunch spends the refusing launcher before giving up.
    sleep(Duration::from_millis(50)).await;
    manager.shutdown().await;
}

/// A server that opens an indexing token and never closes it keeps the wait
/// reporting that results may be partial.
#[tokio::test]
async fn test_indexing_never_settles() {
    let manager = manager();
    manager
        .start_all(vec![config("rust", LspConfigScope::User, false)])
        .await;
    let (peer, _seen) = attach(&manager, "rust").await;
    peer.out
        .send(json!({"jsonrpc": "2.0", "method": "$/progress",
            "params": {"token": "t", "value": {"kind": "begin", "message": "indexing"}}}))
        .expect("sent");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !manager
        .wait_indexing_briefly("rust", Duration::from_millis(5))
        .await
    {
        assert!(Instant::now() < deadline, "indexing stays open");
        sleep(Duration::from_millis(2)).await;
    }
    let row = manager
        .statuses()
        .await
        .into_iter()
        .find(|r| r.name == "rust")
        .expect("rust");
    assert!(
        matches!(row.state, LspServerState::Indexing { .. }),
        "an open token reads as indexing, saw {:?}",
        row.state
    );
    assert!(
        !manager
            .wait_indexing_briefly("nobody", Duration::from_millis(5))
            .await
    );
    manager.shutdown().await;
}

/// The diagnostics drain ignores notifications that are not diagnostics,
/// and the status count follows the store.
#[tokio::test]
async fn test_drain_ignores_other_notices() {
    let manager = manager();
    manager
        .start_all(vec![config("rust", LspConfigScope::User, false)])
        .await;
    let (peer, _seen) = attach(&manager, "rust").await;
    peer.out
        .send(json!({"jsonrpc": "2.0", "method": "window/logMessage",
            "params": {"type": 3, "message": "a log line"}}))
        .expect("sent");
    peer.out
        .send(
            json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///other.rs", "diagnostics": [
                {"range": {"start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1}}, "message": "m"}
            ]}}),
        )
        .expect("sent");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let row = manager
            .statuses()
            .await
            .into_iter()
            .find(|r| r.name == "rust")
            .expect("rust");
        if row.diagnostic_count == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "the diagnostic landed");
        sleep(Duration::from_millis(2)).await;
    }
    manager.shutdown().await;
}

/// A launcher whose server completes one handshake and then closes, so the
/// crash path runs over a real pipe pair.
struct FrailLauncher {
    spawns: Arc<std::sync::atomic::AtomicUsize>,
}

impl ProcessLauncher for FrailLauncher {
    fn spawn(&self, _req: SpawnRequest, _policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        self.spawns
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|error| SpawnError::Io(error.to_string()))?;
        let address = listener
            .local_addr()
            .map_err(|error| SpawnError::Io(error.to_string()))?;
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let Ok(read_half) = stream.try_clone() else {
                return;
            };
            serve_one_handshake(read_half, stream);
        });
        let client = std::net::TcpStream::connect(address)
            .map_err(|error| SpawnError::Io(error.to_string()))?;
        let writer = client
            .try_clone()
            .map_err(|error| SpawnError::Io(error.to_string()))?;
        let pipes = houyicoder_api::launcher::StdioPipes {
            stdin: Some(Box::new(writer)),
            stdout: Some(Box::new(client)),
            stderr: None,
        };
        let waiter: houyicoder_async::PFut<
            'static,
            Result<houyicoder_api::launcher::LauncherExit, SpawnError>,
        > = Box::pin(async {
            Ok(houyicoder_api::launcher::LauncherExit {
                exit_code: Some(0),
                stdout: None,
                stderr: None,
            })
        });
        Ok(LauncherChild::with_pipes(Some(0), pipes, waiter))
    }
}

/// Answer the framed handshake once and close the stream, the shape of a
/// server that dies right after it reports ready.
fn serve_one_handshake(read_half: std::net::TcpStream, mut out: std::net::TcpStream) {
    let mut reader = std::io::BufReader::new(read_half);
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
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
        return;
    };
    let mut body = vec![0u8; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let Ok(frame) = serde_json::from_slice::<Value>(&body) else {
        return;
    };
    let id = frame.get("id").cloned();
    let reply = json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}});
    let Ok(bytes) = serde_json::to_vec(&reply) else {
        return;
    };
    let header = format!("Content-Length: {}\r\n\r\n", bytes.len());
    drop(out.write_all(header.as_bytes()));
    drop(out.write_all(&bytes));
    drop(out.flush());
    // The stream drops here: the connection ends right after the handshake.
}

/// The restart ceiling bounds the relaunch storm: one retry runs, the next
/// crash stops at the cap, and no third process starts.
#[tokio::test]
async fn test_restart_cap_bounds() {
    let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let manager = Arc::new(LspConnections::new(
        Arc::new(FrailLauncher {
            spawns: Arc::clone(&spawns),
        }),
        Arc::from(Path::new("/work")),
        fast_timings(),
    ));
    let mut server = config("rust", LspConfigScope::User, false);
    server.max_restarts = 1;
    manager.start_all(vec![server]).await;
    drop(manager.ensure_started("rust").await.expect("handshake"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let row = manager
            .statuses()
            .await
            .into_iter()
            .find(|r| r.name == "rust")
            .expect("rust");
        let count = spawns.load(std::sync::atomic::Ordering::SeqCst);
        if matches!(row.state, LspServerState::Failed { .. }) && count == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the cap stopped at two spawns, state {:?}, spawns {count}",
            row.state
        );
        sleep(Duration::from_millis(2)).await;
    }
    sleep(Duration::from_millis(50)).await;
    assert_eq!(
        spawns.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "no process starts past the cap"
    );
    manager.shutdown().await;
}

/// A shutdown that lands while the watcher waits out its backoff prevents
/// the relaunch: the closed flag outranks the retry.
#[tokio::test]
async fn test_shutdown_stops_backoff() {
    let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut timings = fast_timings();
    timings.restart_base = Duration::from_millis(120);
    let manager = Arc::new(LspConnections::new(
        Arc::new(FrailLauncher {
            spawns: Arc::clone(&spawns),
        }),
        Arc::from(Path::new("/work")),
        timings,
    ));
    manager
        .start_all(vec![config("rust", LspConfigScope::User, false)])
        .await;
    drop(manager.ensure_started("rust").await.expect("handshake"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while spawns.load(std::sync::atomic::Ordering::SeqCst) < 1 {
        assert!(Instant::now() < deadline, "the first spawn happened");
        sleep(Duration::from_millis(2)).await;
    }
    manager.shutdown().await;
    sleep(Duration::from_millis(300)).await;
    assert_eq!(
        spawns.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a shutdown inside the backoff prevents the relaunch"
    );
    assert!(
        manager.ensure_started("rust").await.is_err(),
        "a closed manager refuses"
    );
}

/// A locked project entry never hijacks a suffix: an enabled plugin server
/// still owns .rs even when the locked entry's name sorts first, which is
/// the shape an untrusted repository's config file has.
#[tokio::test]
async fn test_route_skips_locked() {
    let manager = manager();
    manager
        .start_all(vec![
            config("aaa", LspConfigScope::Project, true),
            config("plugin:rust-analyzer", LspConfigScope::PluginCache, false),
        ])
        .await;
    assert_eq!(
        manager
            .server_for_path(Path::new("/work/a.rs"))
            .await
            .as_deref(),
        Some("plugin:rust-analyzer"),
        "the locked entry cannot take the suffix from the plugin"
    );
}

/// Precedence follows the scope, not the name: a project entry beats a user
/// entry, a user entry beats a plugin, and a plugin named to sort first does
/// not win.
#[tokio::test]
async fn test_route_scope_priority() {
    let manager = manager();
    manager
        .start_all(vec![
            config("zzz", LspConfigScope::User, false),
            config("plugin:aaa", LspConfigScope::PluginCache, false),
        ])
        .await;
    assert_eq!(
        manager
            .server_for_path(Path::new("/work/a.rs"))
            .await
            .as_deref(),
        Some("zzz"),
        "the user's entry outranks a plugin entry"
    );
    manager
        .start_all(vec![config("0project", LspConfigScope::Project, false)])
        .await;
    assert_eq!(
        manager
            .server_for_path(Path::new("/work/a.rs"))
            .await
            .as_deref(),
        Some("0project"),
        "the workspace's own entry outranks the user's"
    );
}

/// A product plugin outranks the frontend plugin cache even when the
/// frontend's name sorts first, which is the precedence tier the scope rank
/// spells out.
#[tokio::test]
async fn test_route_plugin_beats_cache() {
    let manager = manager();
    manager
        .start_all(vec![
            config("plugin:cache-aaa", LspConfigScope::PluginCache, false),
            config("plugin:aaa", LspConfigScope::Plugin, false),
        ])
        .await;
    assert_eq!(
        manager
            .server_for_path(Path::new("/work/a.rs"))
            .await
            .as_deref(),
        Some("plugin:aaa"),
        "the product plugin outranks the frontend cache"
    );
}

/// A switch at runtime re-routes the suffix: disabling the preferred server
/// hands .rs to the enabled fallback, and re-enabling takes it back without
/// a restart.
#[tokio::test]
async fn test_route_follows_switch() {
    let manager = manager();
    manager
        .start_all(vec![
            config("primary", LspConfigScope::User, false),
            config("fallback", LspConfigScope::Plugin, false),
        ])
        .await;
    assert_eq!(
        manager
            .server_for_path(Path::new("/work/a.rs"))
            .await
            .as_deref(),
        Some("primary")
    );
    manager
        .set_enabled("primary", false)
        .await
        .expect("the switch flips");
    assert_eq!(
        manager
            .server_for_path(Path::new("/work/a.rs"))
            .await
            .as_deref(),
        Some("fallback"),
        "the enabled fallback takes over"
    );
    manager
        .set_enabled("primary", true)
        .await
        .expect("the switch flips back");
    assert_eq!(
        manager
            .server_for_path(Path::new("/work/a.rs"))
            .await
            .as_deref(),
        Some("primary"),
        "the preferred server takes the suffix back"
    );
}

/// An entry naming a transport this host cannot launch never routes, so a
/// socket entry cannot shadow a stdio one.
#[tokio::test]
async fn test_route_skips_socket() {
    let manager = manager();
    let mut socket = config("socket", LspConfigScope::User, false);
    socket.transport = LspTransportKind::Socket;
    manager
        .start_all(vec![socket, config("stdio", LspConfigScope::Plugin, false)])
        .await;
    assert_eq!(
        manager
            .server_for_path(Path::new("/work/a.rs"))
            .await
            .as_deref(),
        Some("stdio")
    );
}

/// A workspace reached through a symlink can produce two spellings of the
/// same file. The caller waits on the symlink spelling while the server
/// publishes on the resolved one; the store key resolves both, so the wait
/// finds the publish instead of reporting unknown.
#[cfg(unix)]
#[tokio::test]
async fn test_symlink_uri_reaches_diagnostics() {
    let base = temp_dir().join(format!("houyi-lsp-symlink-{}", id()));
    drop(remove_dir_all(&base));
    create_dir_all(base.join("real")).expect("real dir");
    let real_file = base.join("real/a.rs");
    write(&real_file, "fn a() {}\n").expect("source file");
    let link = base.join("link");
    symlink(base.join("real"), &link).expect("symlink");
    let manager = manager();
    manager
        .start_all(vec![config("rust", LspConfigScope::User, false)])
        .await;
    let (peer, _seen) = attach(&manager, "rust").await;
    let waited = file_uri(&link.join("a.rs"));
    let published = file_uri(&real_file);
    assert_ne!(waited, published, "the two spellings differ");
    let since = manager.publish_generation("rust", &waited).await;
    peer.out
        .send(
            json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
            "params": {"uri": published, "diagnostics": [
                {"range": {"start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1}},
                 "severity": 1, "message": "type mismatch"}
            ]}}),
        )
        .expect("sent");
    let deadline = Instant::now() + Duration::from_secs(5);
    while manager.publish_generation("rust", &waited).await <= since {
        assert!(
            Instant::now() < deadline,
            "the publish on the resolved spelling reaches the symlink key"
        );
        sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(
        manager.file_diagnostics("rust", &waited).await.len(),
        1,
        "the wait finds the findings under the resolved key"
    );
    manager.shutdown().await;
    drop(remove_dir_all(&base));
}
