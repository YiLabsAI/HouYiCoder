//! Tests for this module. The behaviors that drive a real launch run in the
//! tests tree, where a stub server can be started; these cover the manager's
//! own rules without one.

use std::sync::Arc;

use houyicoder_api::launcher::{
    LauncherChild, ProcessLauncher, SpawnError, SpawnPolicy, SpawnRequest,
};
use houyicoder_protocol::mcp::McpConfigScope;
use tokio::time::sleep;

use super::*;

/// A launcher that refuses to start anything, so a test can prove the
/// manager never asked. Shared with the tool tests, which need a manager that
/// can never launch a process.
pub(crate) struct NoSpawn;

impl ProcessLauncher for NoSpawn {
    fn spawn(&self, _req: SpawnRequest, _policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        Err(SpawnError::Io("no process may start in this test".into()))
    }
}

fn manager(launcher: Arc<dyn ProcessLauncher>) -> McpConnections {
    McpConnections::new(
        launcher,
        Arc::from(std::path::Path::new("/work")),
        McpTimings::default(),
    )
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

/// The production defaults are what the manager claims, a short notice
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

/// A disabled entry is recorded as disabled and never reaches the launcher,
/// whatever its transport. This is what the trust gate produces for an
/// untrusted workspace, so the assertion is that nothing was started.
#[tokio::test]
async fn test_disabled_never_spawns() {
    let manager = manager(Arc::new(NoSpawn));
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
            "{} is disabled, saw {:?}",
            status.name,
            status.state
        );
    }
    manager.shutdown().await;
}

/// The diagnostics accessors answer for a disabled server and for an unknown
/// name, so a status view renders an empty tail and not an error.
#[tokio::test]
async fn test_stderr_accessors() {
    let manager = manager(Arc::new(NoSpawn));
    manager.start_all(vec![config("quiet", true)]).await;
    assert!(manager.stderr_tail("quiet").await.is_empty());
    assert!(manager.stderr_tail("nobody").await.is_empty());
    assert_eq!(manager.stderr_limit(), STDERR_TAIL_LINES);
    manager.shutdown().await;
}

/// A reconnect request for a disabled server asks nobody to launch, and the
/// shutdown still joins.
#[tokio::test]
async fn test_reconnect_disabled_is_quiet() {
    let manager = manager(Arc::new(NoSpawn));
    manager.start_all(vec![config("off", true)]).await;
    manager.reconnect("off").await;
    manager.reconnect("unknown").await;
    let statuses = manager.statuses().await;
    assert!(matches!(
        statuses[0].state,
        McpConnectionState::Disabled { .. }
    ));
    manager.shutdown().await;
}

/// A reconnect request for a server the manifest does not hold is a no-op
/// and the shutdown still joins.
#[tokio::test]
async fn test_reconnect_unknown_is_quiet() {
    let manager = manager(Arc::new(NoSpawn));
    manager.start_all(Vec::new()).await;
    manager.reconnect("unknown").await;
    assert!(manager.statuses().await.is_empty());
    manager.shutdown().await;
}

/// Poll until any server's state matches, or the deadline passes.
async fn any_state(
    manager: &McpConnections,
    predicate: impl Fn(&McpConnectionState) -> bool,
) -> bool {
    let started = tokio::time::Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        if manager
            .statuses()
            .await
            .iter()
            .any(|status| predicate(&status.state))
        {
            return true;
        }
        sleep(Duration::from_millis(2)).await;
    }
    false
}

/// A launch the port refuses leaves the server failed with the port's
/// reason, which is what a status view shows a user who must fix the config.
#[tokio::test]
async fn test_launch_refused_reports_failed() {
    let manager = manager(Arc::new(NoSpawn));
    manager.start_all(vec![config("broken", false)]).await;
    assert!(
        any_state(&manager, |state| matches!(
            state,
            McpConnectionState::Failed { error } if error.contains("no process may start")
        ))
        .await,
        "the launch failure is recorded with its reason"
    );
    manager.shutdown().await;
}

/// The call budget comes from the server's own configuration, and a server
/// the manifest does not hold falls back to the production default.
#[tokio::test]
async fn test_call_timeout_names_config() {
    let manager = manager(Arc::new(NoSpawn));
    manager.start_all(vec![config("timed", true)]).await;
    assert_eq!(
        manager.call_timeout_of("timed").await,
        Duration::from_millis(5_000)
    );
    assert_eq!(
        manager.call_timeout_of("nobody").await,
        Duration::from_secs(60)
    );
    manager.shutdown().await;
}

/// The stop token a watcher holds fires on a manager shutdown, and cancelling
/// the watcher's own copy does not stop the manager.
#[tokio::test]
async fn test_stop_token_fires() {
    let manager = manager(Arc::new(NoSpawn));
    let watcher = manager.watch_stop();
    assert!(!watcher.is_cancelled());
    tokio::spawn({
        let manager = manager.clone();
        async move { manager.shutdown().await }
    });
    watcher.cancelled().await;
    assert!(watcher.is_cancelled());
    let other = manager.watch_stop();
    other.cancel();
    assert!(other.is_cancelled());
    assert!(manager.statuses().await.is_empty());
}
