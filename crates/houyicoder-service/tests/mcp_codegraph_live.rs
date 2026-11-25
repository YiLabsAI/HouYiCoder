//! Live integration with the local code graph server, run only by the live
//! suite. It needs the checkout built and a Node runtime the server accepts
//! (it refuses Node 25 and newer). HOUYICODER_CODEGRAPH_NODE and
//! HOUYICODER_CODEGRAPH_HOME name the prerequisites, and
//! HOUYICODER_CODEGRAPH_WORKSPACE names the project to index; a missing
//! prerequisite skips with the reason, and nothing machine-specific lives in
//! this file.

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_api::tool::ToolCtx;
use houyicoder_core::agent::ToolSource;
use houyicoder_permission::{DefaultModeGate, ModeGate};
use houyicoder_protocol::mcp::{
    McpConfigScope, McpConnectionState, McpTransportConfig, ScopedMcpServerConfig,
};
use houyicoder_service::mcp::{McpConnections, McpTimings, McpToolSource, RestartPolicy};
use serde_json::json;
use tokio::time::sleep;
use tokio::time::timeout;

/// The Node runtime the live run uses. The server refuses Node 25 because
/// its WebAssembly engine crashes on the grammars, so the runtime is named
/// by the environment rather than taken from PATH.
fn node_path() -> Option<PathBuf> {
    let candidate = env::var_os("HOUYICODER_CODEGRAPH_NODE").map(PathBuf::from)?;
    candidate.is_file().then_some(candidate)
}

/// The built code graph checkout the live run launches.
fn entry_path() -> Option<PathBuf> {
    let root = env::var_os("HOUYICODER_CODEGRAPH_HOME").map(PathBuf::from)?;
    let entry = root.join("dist").join("bin").join("codegraph.js");
    entry.is_file().then_some(entry)
}

/// The project the live run indexes: the named directory, or this
/// repository's root derived from the crate's own location so the default
/// names a real tree on any checkout.
fn index_workspace() -> PathBuf {
    env::var_os("HOUYICODER_CODEGRAPH_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .to_path_buf()
        })
}

fn live_timings() -> McpTimings {
    McpTimings {
        notice_window: Duration::from_millis(50),
        handshake_timeout: Duration::from_secs(15),
        liveness_poll: Duration::from_millis(200),
        stable_after: Duration::from_secs(5),
        close_timeout: Duration::from_secs(2),
        restart: RestartPolicy::new(Duration::from_millis(500), Duration::from_secs(5), 2),
    }
}

/// Poll the status view until the server reports connected.
async fn wait_connected(manager: &McpConnections, deadline: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < deadline {
        if manager
            .statuses()
            .await
            .iter()
            .any(|status| matches!(status.state, McpConnectionState::Connected))
        {
            return true;
        }
        sleep(Duration::from_millis(2)).await;
    }
    false
}

/// Attach the real local server. A machine without the checkout or the
/// supported runtime skips with the reason instead of failing the suite.
#[tokio::test]
#[ignore]
async fn test_codegraph_handshake_and_tool() {
    let Some(node) = node_path() else {
        eprintln!(
            "skipped: no supported Node runtime; set HOUYICODER_CODEGRAPH_NODE to a Node 24 binary"
        );
        return;
    };
    let Some(entry) = entry_path() else {
        eprintln!(
            "skipped: the code graph server is not built; run npm ci && npm run build in its checkout"
        );
        return;
    };
    let workspace: Arc<Path> = Arc::from(index_workspace());
    let manager = Arc::new(McpConnections::new(
        Arc::new(StdProcessLauncher::new()),
        workspace,
        live_timings(),
    ));
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let source = manager.tool_source(gate, None);
    let server = server_config(&node, &entry);

    let cold = Instant::now();
    manager.start_all(vec![server], true).await;
    let connected = wait_connected(&manager, Duration::from_secs(15)).await;
    let cold = cold.elapsed();
    if !connected {
        let stderr = manager.stderr_tail("codegraph").await;
        panic!("the server did not connect; stderr tail: {stderr:?}");
    }
    eprintln!("connect (spawn + handshake + list_tools): {cold:?}");
    assert!(
        cold < Duration::from_secs(5),
        "a reverse roots request must not stall the handshake, saw {cold:?}"
    );
    if cold >= Duration::from_millis(300) {
        eprintln!("note: connect exceeded the 300ms warm target (see the recorded baseline)");
    }

    // The source is published by the subscription task after the connection
    // announces, so wait for the tool rather than reading it on the same wake.
    let started = Instant::now();
    while source.get("mcp__codegraph__codegraph_explore").is_none()
        && started.elapsed() < Duration::from_secs(10)
    {
        sleep(Duration::from_millis(2)).await;
    }
    assert_explore(&source).await;

    manager.shutdown().await;
}

fn server_config(node: &Path, entry: &Path) -> ScopedMcpServerConfig {
    ScopedMcpServerConfig {
        name: "codegraph".into(),
        scope: McpConfigScope::User,
        transport: McpTransportConfig::stdio(
            node.display().to_string(),
            vec![
                entry.display().to_string(),
                "serve".to_string(),
                "--mcp".to_string(),
            ],
        ),
        disabled: false,
        tool_timeout_ms: 120_000,
    }
}

/// What the source exposes once the server is connected, and a real call
/// through the read-only tool.
async fn assert_explore(source: &McpToolSource) {
    let names = source.names();
    assert!(!names.is_empty(), "the server exposes at least one tool");
    assert!(
        names
            .iter()
            .all(|name| name.starts_with("mcp__codegraph__")),
        "every exposed tool carries the server namespace, saw {names:?}"
    );
    let explore = source
        .get("mcp__codegraph__codegraph_explore")
        .expect("the explore tool is exposed");
    let Some(project) = indexed_project() else {
        eprintln!(
            "skipped the call: no indexed workspace; run codegraph init in the checkout and set HOUYICODER_CODEGRAPH_WORKSPACE"
        );
        return;
    };
    let input = json!({"query": "SessionStore", "projectPath": project.display().to_string()});
    assert!(
        !explore.requires_approval_for(&input),
        "the read-only hint lets the call through without a card"
    );
    let instructions = source
        .instructions()
        .expect("the server's instructions reach the prompt");
    assert!(
        instructions.contains("<mcp_server_instructions server=\"codegraph\">"),
        "the instructions arrive inside the server tag"
    );
    let call = timeout(
        Duration::from_secs(120),
        explore.execute(ToolCtx::new("live"), input),
    )
    .await;
    let value = match call {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => panic!("the explore call failed: {error}"),
        Err(_) => panic!("the explore call did not answer inside its budget"),
    };
    let rendered = value.to_string();
    assert!(
        !rendered.contains("isn't indexed"),
        "the call answered from the index, saw {rendered}"
    );
    eprintln!("explore returned {} bytes", rendered.len());
}

/// The workspace the call queries: the named project or this repository's
/// root, and only when that tree carries an index.
fn indexed_project() -> Option<PathBuf> {
    let project = index_workspace();
    project.join(".codegraph").is_dir().then_some(project)
}
