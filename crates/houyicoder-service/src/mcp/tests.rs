//! Tests for this module. The runtime is started over a disabled entry, so
//! the composition path runs end to end while no process may launch.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use houyicoder_api::session::SessionLog;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::{DefaultModeGate, ModeGate};
use houyicoder_protocol::mcp::{
    McpConfigScope, McpConnectionState, McpTransportConfig, ScopedMcpServerConfig,
};
use houyicoder_session::SessionStore;
use serde_json::json;
use tokio::runtime::Handle;
use tokio::time::Instant;
use tokio::time::sleep;

use super::*;

fn auto_gate() -> Arc<dyn ModeGate> {
    Arc::new(DefaultModeGate::new())
}

fn session_log() -> Arc<dyn SessionLog> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    store as Arc<dyn SessionLog>
}

fn config(name: &str, disabled: bool) -> ScopedMcpServerConfig {
    ScopedMcpServerConfig {
        name: name.into(),
        scope: McpConfigScope::User,
        transport: McpTransportConfig::stdio("fake", vec![]),
        disabled,
        tool_timeout_ms: 5_000,
    }
}

/// A resolved list with nothing in it starts nothing, so an unconfigured
/// session pays no manager, no mirror, and no background task.
#[tokio::test]
async fn test_empty_list_yields_none() {
    let handle = Handle::current();
    assert!(start_resolved(&handle, Vec::new(), None, auto_gate(), session_log()).is_none());
}

/// A configured entry that is disabled still yields the runtime a status view
/// reads, and its state says why nothing is running.
#[tokio::test]
async fn test_disabled_entry_yields_runtime() {
    let handle = Handle::current();
    let runtime = start_resolved(
        &handle,
        vec![config("quiet", true)],
        None,
        auto_gate(),
        session_log(),
    )
    .expect("a configured server yields a runtime");
    let started = Instant::now();
    let mut statuses = runtime.manager.statuses().await;
    while statuses.is_empty() && started.elapsed() < Duration::from_secs(2) {
        sleep(Duration::from_millis(2)).await;
        statuses = runtime.manager.statuses().await;
    }
    assert_eq!(statuses.len(), 1);
    assert!(
        matches!(statuses[0].state, McpConnectionState::Disabled { .. }),
        "the disabled entry reports why it is not running, saw {:?}",
        statuses[0].state
    );
    assert!(runtime.source.names().is_empty());
    runtime.manager.shutdown().await;
}

/// The settings file is the entry a session without a project workspace
/// reads, and one entry there is enough to give the registry a live source.
#[tokio::test]
async fn test_settings_entry_starts_runtime() {
    let root = unique_dir();
    let settings = root.join("settings.json");
    fs::create_dir_all(&root).expect("the temporary home is writable");
    fs::write(
        &settings,
        json!({"mcpServers": {"quiet": {"command": "fake", "disabled": true}}}).to_string(),
    )
    .expect("the settings file is written");
    let runtime = start_servers(
        &settings,
        None,
        true,
        None,
        Some(&Handle::current()),
        auto_gate(),
        session_log(),
    )
    .expect("the settings entry yields a runtime");
    let started = Instant::now();
    let mut found = false;
    while !found && started.elapsed() < Duration::from_secs(2) {
        found = runtime
            .manager
            .statuses()
            .await
            .iter()
            .any(|status| status.name == "quiet");
        sleep(Duration::from_millis(2)).await;
    }
    assert!(found, "the configured server appears in the status view");
    runtime.manager.shutdown().await;
    drop(fs::remove_dir_all(&root));
}

/// A directory no other test in this process shares, under the OS temp root.
fn unique_dir() -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!("houyi-mcp-{}-{n}", process::id()))
}

/// A workspace the user has not trusted contributes no server: its project
/// file is read and disabled before the manager sees it, and the source the
/// registry reads stays empty.
#[tokio::test]
async fn test_untrusted_project_server_disabled() {
    let root = unique_dir();
    let workspace = root.join("repo");
    fs::create_dir_all(&workspace).expect("the workspace is writable");
    fs::write(
        workspace.join(".mcp.json"),
        json!({"mcpServers": {"sneaky": {"command": "fake"}}}).to_string(),
    )
    .expect("the project file is written");
    let settings = root.join("settings.json");
    let runtime = start_servers(
        &settings,
        Some(&workspace),
        false,
        None,
        Some(&Handle::current()),
        auto_gate(),
        session_log(),
    )
    .expect("the project server is still reported, as disabled");
    let started = Instant::now();
    let mut statuses = runtime.manager.statuses().await;
    while statuses.is_empty() && started.elapsed() < Duration::from_secs(2) {
        sleep(Duration::from_millis(2)).await;
        statuses = runtime.manager.statuses().await;
    }
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].name, "sneaky");
    assert!(
        matches!(statuses[0].state, McpConnectionState::Disabled { .. }),
        "an untrusted workspace's server is disabled, saw {:?}",
        statuses[0].state
    );
    assert!(runtime.source.names().is_empty());
    runtime.manager.shutdown().await;
    drop(fs::remove_dir_all(&root));
}
