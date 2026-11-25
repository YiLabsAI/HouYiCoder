//! Cross-layer tests for the external tool server client. Each test starts a
//! stub server through the real launcher path, so the spawn, the pipe bridge,
//! the transport handshake, and the manager's supervision all run as they do
//! for a real server. The stub speaks the protocol over a loopback socket.

#[path = "common/mcp_server.rs"]
mod mcp_server;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use houyicoder_api::tool::ToolCtx;
use houyicoder_core::agent::ToolSource;
use houyicoder_permission::{DefaultModeGate, ModeGate};
use houyicoder_protocol::mcp::{
    McpConfigScope, McpConnectionState, McpTransportConfig, ScopedMcpServerConfig,
};
use houyicoder_service::mcp::{McpConnections, McpTimings, RestartPolicy};
use mcp_server::{Behavior, Counters, SocketLauncher};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

fn fast_timings() -> McpTimings {
    McpTimings {
        notice_window: Duration::from_millis(30),
        handshake_timeout: Duration::from_millis(500),
        liveness_poll: Duration::from_millis(5),
        stable_after: Duration::from_millis(20),
        close_timeout: Duration::from_millis(200),
        restart: RestartPolicy::new(Duration::from_millis(1), Duration::from_millis(2), 2),
    }
}

fn manager_with(
    behaviors: Vec<Behavior>,
    timings: McpTimings,
) -> (Arc<McpConnections>, Arc<Counters>) {
    let launcher = Arc::new(SocketLauncher::new(behaviors));
    let counters = Arc::clone(&launcher.counters);
    let manager = Arc::new(McpConnections::new(
        launcher,
        Arc::from(Path::new("/work")),
        timings,
    ));
    (manager, counters)
}

fn auto_gate() -> Arc<dyn ModeGate> {
    Arc::new(DefaultModeGate::new())
}

fn config(name: &str, disabled: bool) -> ScopedMcpServerConfig {
    ScopedMcpServerConfig {
        name: name.into(),
        scope: McpConfigScope::Project,
        transport: McpTransportConfig::stdio("fake", vec![]),
        disabled,
        tool_timeout_ms: 5_000,
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

async fn status_is(
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

/// An untrusted workspace's servers are disabled before the manager sees
/// them, and a disabled entry never reaches the launcher. The spawn count
/// stays zero for both child-process and endpoint entries.
#[tokio::test]
async fn test_untrusted_never_spawns() {
    let (manager, counters) = manager_with(Vec::new(), fast_timings());
    let mut gated = vec![config("spawny", false), config("remote", false)];
    gated[1].transport = McpTransportConfig::http("https://example.invalid/mcp");
    let gated = houyicoder_config::apply_trust_gate(gated, false);
    manager.start_all(gated, false).await;
    let statuses = manager.statuses().await;
    assert_eq!(statuses.len(), 2);
    for status in &statuses {
        assert!(
            matches!(status.state, McpConnectionState::Disabled { .. }),
            "{} is disabled, saw {:?}",
            status.name,
            status.state
        );
    }
    assert_eq!(
        counters.spawns.load(Ordering::SeqCst),
        0,
        "no process starts for an untrusted workspace"
    );
    manager.shutdown().await;
}

/// A burst of change notices inside the window folds into one refetch. The
/// server sees the initial listing plus exactly one more, and a second
/// window later does not follow.
#[tokio::test]
async fn test_list_changed_coalesced() {
    let (manager, counters) = manager_with(vec![Behavior::Burst], fast_timings());
    manager.start_all(vec![config("chatty", false)], true).await;
    assert!(
        eventually(|| counters.tool_lists.load(Ordering::SeqCst) == 2).await,
        "the burst produced one refetch, saw {} listings",
        counters.tool_lists.load(Ordering::SeqCst)
    );
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
    manager.start_all(vec![config("flaky", false)], true).await;
    assert!(
        status_is(&manager, |state| matches!(
            state,
            McpConnectionState::Failed { .. }
        ))
        .await,
        "the server reaches a failed state"
    );
    assert_eq!(
        counters.spawns.load(Ordering::SeqCst),
        3,
        "the policy's two restarts plus the first launch"
    );
    manager.shutdown().await;
}

/// A reconnect request launches a failed server again, and a graceful
/// shutdown ends every task.
#[tokio::test]
async fn test_reconnect_after_failure() {
    let behaviors = vec![Behavior::CloseNow, Behavior::CloseNow, Behavior::CloseNow];
    let (manager, counters) = manager_with(behaviors, fast_timings());
    manager.start_all(vec![config("flaky", false)], true).await;
    assert!(
        eventually(|| counters.spawns.load(Ordering::SeqCst) == 3).await,
        "the retries ran"
    );
    assert!(
        status_is(&manager, |state| matches!(
            state,
            McpConnectionState::Failed { .. }
        ))
        .await,
        "the server is failed before the reconnect"
    );
    manager.reconnect("flaky").await;
    assert!(
        eventually(|| counters.spawns.load(Ordering::SeqCst) == 4).await,
        "the reconnect started the server again"
    );
    manager.shutdown().await;
}

/// The tools the registry sees are ordered by server name and then tool
/// name, so the list does not depend on which handshake finished first. A
/// tool the server declared without annotations arrives behind the mode gate
/// and asks, so nothing external reaches the runner unguarded.
#[tokio::test]
async fn test_tools_ordered_and_gated() {
    let (manager, _counters) = manager_with(Vec::new(), fast_timings());
    let source = manager.tool_source(auto_gate(), None);
    manager
        .start_all(vec![config("beta", false), config("alpha", false)], true)
        .await;
    assert!(
        eventually(|| source.names().len() == 4).await,
        "both servers connect and appear"
    );
    assert_eq!(
        source.names(),
        vec![
            "mcp__alpha__probe".to_string(),
            "mcp__alpha__wipe".to_string(),
            "mcp__beta__probe".to_string(),
            "mcp__beta__wipe".to_string()
        ]
    );
    let wipe = source
        .get("mcp__alpha__wipe")
        .expect("the write tool is exposed");
    assert!(
        wipe.requires_approval_for(&json!({})),
        "an unannotated external tool reaches the user as a card"
    );
    let probe = source
        .get("mcp__alpha__probe")
        .expect("the read tool is exposed");
    assert!(
        !probe.requires_approval_for(&json!({})),
        "a read-only external tool runs without a card"
    );
    manager.shutdown().await;
}

/// A server that connects exposes its tools through the source the registry
/// reads, and a call on one reaches the server and returns its text.
#[tokio::test]
async fn test_call_returns_text() {
    let (manager, counters) = manager_with(Vec::new(), fast_timings());
    let source = manager.tool_source(auto_gate(), None);
    manager.start_all(vec![config("graph", false)], true).await;
    assert!(
        eventually(|| source.names().len() == 2).await,
        "the server connects and its tools appear"
    );
    let probe = source
        .get("mcp__graph__probe")
        .expect("the read tool is exposed");
    assert!(probe.is_read_only());
    let out = probe
        .execute(ToolCtx::new("c1"), json!({"q": "x"}))
        .await
        .expect("the call succeeds");
    assert_eq!(out, Value::String("answer".to_string()));
    assert_eq!(counters.tool_calls.load(Ordering::SeqCst), 1);
    let instructions = source.instructions().expect("server instructions present");
    assert!(instructions.contains("use probe"));
    manager.shutdown().await;
}

/// A server that drops is restarted, and the exposed tools move to the new
/// connection, so a call after the restart reaches the live process instead
/// of the peer that died.
#[tokio::test]
async fn test_reconnect_swaps_tools() {
    let (manager, counters) =
        manager_with(vec![Behavior::CloseNow, Behavior::Calm], fast_timings());
    let source = manager.tool_source(auto_gate(), None);
    manager.start_all(vec![config("flaky", false)], true).await;
    assert!(
        eventually(|| counters.spawns.load(Ordering::SeqCst) == 2).await,
        "the dropped server was restarted"
    );
    assert!(
        status_is(&manager, |state| matches!(
            state,
            McpConnectionState::Connected
        ))
        .await,
        "the restarted server connects"
    );
    let mut answer = None;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        match source.get("mcp__flaky__probe") {
            Some(probe) => match probe.execute(ToolCtx::new("c1"), json!({})).await {
                Ok(value) => {
                    answer = Some(value);
                    break;
                }
                Err(_) => sleep(Duration::from_millis(2)).await,
            },
            None => sleep(Duration::from_millis(2)).await,
        }
    }
    assert_eq!(
        answer,
        Some(Value::String("answer".to_string())),
        "the call reached the restarted server"
    );
    manager.shutdown().await;
}

/// A stop during an in-flight call ends the wait instead of hanging the
/// turn: the call resolves with an error once the manager shuts down.
#[tokio::test]
async fn test_shutdown_bounded() {
    let (manager, counters) = manager_with(vec![Behavior::HangCall], fast_timings());
    let source = manager.tool_source(auto_gate(), None);
    manager.start_all(vec![config("slow", false)], true).await;
    assert!(
        eventually(|| source.names().len() == 2).await,
        "the server connects and its tools appear"
    );
    let probe = source
        .get("mcp__slow__probe")
        .expect("the read tool is exposed");
    let call = tokio::spawn(async move { probe.execute(ToolCtx::new("c1"), json!({})).await });
    assert!(
        eventually(|| counters.tool_calls.load(Ordering::SeqCst) == 1).await,
        "the request reached the server and is waiting for an answer"
    );
    timeout(Duration::from_secs(5), manager.shutdown())
        .await
        .expect("shutdown returns inside its budget");
    let outcome = timeout(Duration::from_secs(5), call)
        .await
        .expect("the in-flight call resolves once the manager stops")
        .expect("the call task does not panic");
    assert!(
        outcome.is_err(),
        "a call cut short by shutdown reports an error, saw {outcome:?}"
    );
}
