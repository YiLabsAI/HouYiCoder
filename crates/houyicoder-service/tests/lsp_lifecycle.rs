//! Cross-layer tests for the language server client: lazy starts, the
//! single-flight handshake, the transient-error retry, indexing progress,
//! strictly increasing document versions, crash relaunch, and the orderly
//! stop. Each test runs against a stub server over a loopback socket through
//! the real launcher path.

#[path = "common/lsp_server.rs"]
mod lsp_server;

#[cfg(unix)]
use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_protocol::lsp::{LspConfigScope, LspServerState, ScopedLspServerConfig};
use serde_json::json;
#[cfg(unix)]
use std::env::temp_dir;
#[cfg(unix)]
use std::fs::create_dir_all;
#[cfg(unix)]
use std::fs::read_to_string;
#[cfg(unix)]
use std::fs::remove_dir_all;
#[cfg(unix)]
use std::fs::write;
use std::path::Path;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::process::id;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::time::sleep;

use houyicoder_service::lsp::{LspConnections, LspTimings};
use lsp_server::{Behavior, Counters, SocketLauncher};

fn fast_timings() -> LspTimings {
    LspTimings {
        retry_delays: [
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(4),
        ],
        restart_base: Duration::from_millis(10),
        diagnostics_grace: Duration::from_millis(2),
        diagnostics_cap: Duration::from_millis(10),
        index_wait: Duration::from_millis(5),
        index_poll: Duration::from_millis(2),
        stable_after: Duration::from_millis(20),
    }
}

fn manager_with(behaviors: Vec<Behavior>) -> (Arc<LspConnections>, Arc<Counters>) {
    let launcher = Arc::new(SocketLauncher::new(behaviors));
    let counters = Arc::clone(&launcher.counters);
    let manager = Arc::new(LspConnections::new(
        launcher,
        Arc::from(Path::new("/work")),
        fast_timings(),
    ));
    (manager, counters)
}

fn config(name: &str) -> ScopedLspServerConfig {
    let mut server = ScopedLspServerConfig::stdio("fake", Default::default());
    server.name = name.into();
    server.scope = LspConfigScope::User;
    server
        .extension_to_language
        .insert(".rs".into(), "rust".into());
    server
}

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

/// Settling registers the server and starts nothing; the first file the
/// suffix routes launches it, opens the document with version one, and every
/// later sync sends the next version.
#[tokio::test]
async fn test_lazy_start_and_versions() {
    let (manager, counters) = manager_with(vec![Behavior::Calm]);
    manager.start_all(vec![config("rust")]).await;
    assert_eq!(
        counters.spawns.load(Ordering::SeqCst),
        0,
        "no process at settle"
    );
    let statuses = manager.statuses().await;
    assert_eq!(statuses[0].state, LspServerState::Idle);

    let file = Path::new("/work/src/lib.rs");
    manager
        .sync_file_saved(file, "fn a() {}")
        .await
        .expect("first sync")
        .expect("a server routes the suffix");
    assert_eq!(counters.spawns.load(Ordering::SeqCst), 1, "lazy start");
    manager
        .sync_file_saved(file, "fn a() { changed }")
        .await
        .expect("second sync");
    manager
        .sync_file_saved(file, "fn a() { changed twice }")
        .await
        .expect("third sync");
    assert!(
        eventually(|| counters.change_versions().len() == 2).await,
        "two changes reached the server"
    );
    assert_eq!(counters.change_versions(), vec![2, 3], "versions increase");
    let opens = counters
        .documents
        .lock()
        .expect("documents")
        .iter()
        .filter(|(method, _)| method == "textDocument/didOpen")
        .count();
    assert_eq!(opens, 1, "one open per document");
    manager.shutdown().await;
}

/// Concurrent first asks share one handshake: the single lock spans the
/// launch, so the launcher starts one child, not two.
#[tokio::test]
async fn test_concurrent_asks_share_handshake() {
    let (manager, counters) = manager_with(vec![Behavior::Calm]);
    manager.start_all(vec![config("rust")]).await;
    let file = Path::new("/work/src/lib.rs");
    let first = Arc::clone(&manager);
    let second = Arc::clone(&manager);
    let a = tokio::spawn(async move { first.sync_file_saved(file, "a").await });
    let b = tokio::spawn(async move { second.sync_file_saved(file, "b").await });
    drop(a.await.expect("ask completes"));
    drop(b.await.expect("ask completes"));
    assert_eq!(
        counters.spawns.load(Ordering::SeqCst),
        1,
        "one server for two concurrent asks"
    );
    assert_eq!(counters.initializes.load(Ordering::SeqCst), 1);
    manager.shutdown().await;
}

/// A request answered with the transient state-changed error is retried on
/// the configured schedule and then succeeds.
#[tokio::test]
async fn test_transient_error_retries() {
    let (manager, counters) = manager_with(vec![Behavior::ModifiedThenOk]);
    manager.start_all(vec![config("rust")]).await;
    let answer = manager
        .request_for_file(
            Path::new("/work/src/lib.rs"),
            "textDocument/hover",
            json!({}),
            Some("fn probe() {}"),
            Duration::from_secs(5),
        )
        .await
        .expect("eventually answers");
    assert_eq!(
        answer.get("contents").and_then(|c| c.get("value")),
        Some(&json!("hover answer"))
    );
    assert_eq!(
        counters.calls_of("textDocument/hover"),
        4,
        "three refusals and the answer"
    );
    manager.shutdown().await;
}

/// A server that opens an indexing progress token reads as indexing, and the
/// wait returns as soon as the token closes.
#[tokio::test]
async fn test_indexing_wait() {
    let (manager, _counters) = manager_with(vec![Behavior::ProgressThenEnd]);
    manager.start_all(vec![config("rust")]).await;
    manager
        .sync_file_saved(Path::new("/work/src/lib.rs"), "fn a() {}")
        .await
        .expect("started");
    let started = Instant::now();
    let mut saw_indexing = false;
    while started.elapsed() < Duration::from_secs(5) {
        let statuses = manager.statuses().await;
        if matches!(statuses[0].state, LspServerState::Indexing { .. }) {
            saw_indexing = true;
            break;
        }
        sleep(Duration::from_millis(2)).await;
    }
    assert!(saw_indexing, "the progress token opened");
    let still = manager
        .wait_indexing_briefly("rust", Duration::from_secs(2))
        .await;
    assert!(!still, "the wait returns once indexing ends");
    manager.shutdown().await;
}

/// A connection that ends on its own is relaunched within the configured
/// bounds, and the slot reports the relaunch.
#[tokio::test]
async fn test_crash_relaunch() {
    let (manager, counters) = manager_with(vec![Behavior::ExitAfterInit, Behavior::Calm]);
    manager.start_all(vec![config("rust")]).await;
    manager
        .sync_file_saved(Path::new("/work/src/lib.rs"), "fn a() {}")
        .await
        .expect("started");
    assert!(
        eventually(|| counters.spawns.load(Ordering::SeqCst) == 2).await,
        "the crashed server relaunched"
    );
    let started = Instant::now();
    let mut running = false;
    while started.elapsed() < Duration::from_secs(5) {
        let statuses = manager.statuses().await;
        if statuses[0].state == LspServerState::Running {
            running = true;
            break;
        }
        sleep(Duration::from_millis(2)).await;
    }
    assert!(running, "the relaunched server reports running");
    manager.shutdown().await;
}

/// A graceful stop closes the stub server's connection: the socket loop
/// ends, so nothing outlives the manager.
#[tokio::test]
async fn test_shutdown_closes_connection() {
    let (manager, counters) = manager_with(vec![Behavior::Calm]);
    manager.start_all(vec![config("rust")]).await;
    manager
        .sync_file_saved(Path::new("/work/src/lib.rs"), "fn a() {}")
        .await
        .expect("sync");
    manager.shutdown().await;
    assert!(
        eventually(|| counters.live.load(Ordering::SeqCst) == 0).await,
        "the connection ended with the manager"
    );
}

/// A switch turned off while a launch is in flight wins: the fresh
/// connection is closed instead of stored, the row reads disabled, and the
/// next ask refuses.
#[tokio::test]
async fn test_disable_during_launch_sticks() {
    let (manager, counters) = manager_with(vec![Behavior::SlowInit]);
    manager.start_all(vec![config("rust")]).await;
    let launch = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            manager
                .ensure_document(Path::new("/work/src/lib.rs"), "fn a() {}")
                .await
        })
    };
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5)
        && manager.statuses().await[0].state != LspServerState::Starting
    {
        sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(
        manager.statuses().await[0].state,
        LspServerState::Starting,
        "the launch is in flight"
    );
    manager
        .set_enabled("rust", false)
        .await
        .expect("the switch flips");
    let outcome = launch.await.expect("the launch task joins");
    assert!(
        outcome.is_err(),
        "the switch wins over the in-flight launch: {outcome:?}"
    );
    assert!(
        matches!(
            manager.statuses().await[0].state,
            LspServerState::Disabled { .. }
        ),
        "the row stays off"
    );
    assert!(
        eventually(|| counters.live.load(Ordering::SeqCst) == 0).await,
        "the fresh connection was closed"
    );
    // A disabled server no longer routes the suffix, so the document path
    // reports no route rather than an error: the user asked for a file
    // nothing serves.
    let routed = manager
        .ensure_document(Path::new("/work/src/lib.rs"), "fn a() {}")
        .await;
    assert!(
        routed.as_ref().is_ok_and(|outcome| outcome.is_none()),
        "a disabled server routes no suffix: {routed:?}"
    );
    manager.shutdown().await;
}

/// A language server that completes the handshake, then ignores both the
/// shutdown request and the exit notification. It is the wedged process the
/// manager's stop must terminate rather than leave behind.
#[cfg(unix)]
const HANG_SERVER: &str = r#"
import json, os, sys, time

pidfile = sys.argv[1]
with open(pidfile, "w") as handle:
    handle.write(str(os.getpid()))

def read_message():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            if length is None:
                continue
            return json.loads(sys.stdin.buffer.read(length))
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1])

def send(payload):
    body = json.dumps(payload).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        # Ignore the closed stream too: this server never stops on its own.
        time.sleep(3600)
        continue
    method = message.get("method", "")
    if "id" not in message:
        continue
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {}}})
    # Every other request is deliberately unanswered: the server wedges.
"#;

/// Whether one process is still present. A reaped process answers failure.
#[cfg(unix)]
#[allow(clippy::disallowed_methods)]
fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A wedged server ignores shutdown and exit. The stop stays bounded, and
/// dropping the child handle runs the launcher's kill-on-drop, which ends the
/// process group instead of leaking it.
#[cfg(unix)]
#[tokio::test]
async fn test_wedged_server_is_killed() {
    let base = temp_dir().join(format!("houyi-lsp-wedged-{}", id()));
    drop(remove_dir_all(&base));
    create_dir_all(&base).expect("probe dir");
    let script = base.join("hang_server.py");
    write(&script, HANG_SERVER).expect("hang server script");
    let pidfile = base.join("pid");
    let file = base.join("a.rs");
    write(&file, "fn a() {}\n").expect("source file");
    let mut server = config("rust");
    server.command = "python3".into();
    server.args = vec![
        script.to_string_lossy().into_owned(),
        pidfile.to_string_lossy().into_owned(),
    ];
    server.shutdown_timeout_ms = 150;
    server.restart_on_crash = false;
    let manager = Arc::new(LspConnections::new(
        Arc::new(StdProcessLauncher::new()),
        Arc::from(base.as_path()),
        fast_timings(),
    ));
    manager.start_all(vec![server]).await;
    manager
        .ensure_document(&file, "fn a() {}\n")
        .await
        .expect("the wedged server handshakes")
        .expect("a server routes the suffix");
    let started = Instant::now();
    while !pidfile.exists() && started.elapsed() < Duration::from_secs(5) {
        sleep(Duration::from_millis(5)).await;
    }
    let pid: u32 = read_to_string(&pidfile)
        .expect("the server writes its pid")
        .trim()
        .parse()
        .expect("a numeric pid");
    assert!(process_alive(pid), "the wedged server runs before the stop");
    let stopped = Instant::now();
    manager.shutdown().await;
    assert!(
        stopped.elapsed() < Duration::from_secs(3),
        "the stop is bounded by the shutdown budget: {:?}",
        stopped.elapsed()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(pid) && Instant::now() < deadline {
        sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !process_alive(pid),
        "the wedged server was killed, not left as an orphan"
    );
    drop(remove_dir_all(&base));
}
