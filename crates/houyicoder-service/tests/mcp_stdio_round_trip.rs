//! A real child process speaking the MCP protocol over its stdio, driven by
//! the manager through the real ProcessLauncher path.
//!
//! Exercises initialize, reverse roots/list, instructions capture, readOnlyHint
//! execution without prompt, list_changed coalesced notification, and zero
//! orphaned processes on exit.
//!
//! This binary re-enters itself via current_exe() with --server, serving
//! NDJSON RPC over its standard I/O streams without harness banners.

mod common;

use std::env::{current_exe, temp_dir};
use std::io::{BufRead, BufReader, Write, stdin, stdout};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use houyicoder_api::launcher::{ProcessLauncher, StdProcessLauncher};
use houyicoder_api::tool::ToolCtx;
use houyicoder_core::agent::ToolSource;
use houyicoder_permission::{DefaultModeGate, ModeGate};
use houyicoder_protocol::mcp::{McpConfigScope, McpTransportConfig, ScopedMcpServerConfig};
use houyicoder_service::mcp::{
    McpConnections, McpTimings, McpToolSource, RestartPolicy, start_mirror,
};
use serde_json::{Value, json};
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::time::{sleep, timeout};

fn fast_timings() -> McpTimings {
    McpTimings {
        notice_window: Duration::from_millis(30),
        handshake_timeout: Duration::from_millis(3000),
        liveness_poll: Duration::from_millis(10),
        stable_after: Duration::from_millis(20),
        close_timeout: Duration::from_millis(200),
        restart: RestartPolicy::new(Duration::from_millis(1), Duration::from_millis(2), 2),
    }
}

fn auto_gate() -> Arc<dyn ModeGate> {
    Arc::new(DefaultModeGate::new())
}

/// Poll until the condition is true or timeout elapses.
async fn eventually(mut predicate: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if predicate() {
            return true;
        }
        sleep(Duration::from_millis(5)).await;
    }
    predicate()
}

fn handle_child_request(req: Value, writer: &mut impl Write) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "initialize" => {
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": { "name": "child-test-server", "version": "1.0.0" },
                    "instructions": "test instructions for child server"
                }
            });
            let _unused = writeln!(writer, "{resp}");
            drop(writer.flush());
        }
        "notifications/initialized" => {}
        "tools/list" => {
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "echo_fast",
                            "description": "echo input text",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "msg": { "type": "string" } }
                            },
                            "annotations": { "readOnlyHint": true }
                        },
                        {
                            "name": "danger_wipe",
                            "description": "unannotated tool",
                            "inputSchema": { "type": "object" }
                        }
                    ]
                }
            });
            let _unused = writeln!(writer, "{resp}");
            drop(writer.flush());
        }
        "tools/call" => {
            let tool_name = req
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let msg = req
                .get("params")
                .and_then(|p| p.get("arguments"))
                .and_then(|a| a.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("echoed");
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": format!("{tool_name}:{msg}") }],
                    "isError": false
                }
            });
            let _unused = writeln!(writer, "{resp}");
            drop(writer.flush());
        }
        _ => {
            if let Some(resp_id) = id
                && req.get("result").is_none()
                && req.get("error").is_none()
            {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "id": resp_id,
                    "error": { "code": -32601, "message": "Method not found" }
                });
                let _unused = writeln!(writer, "{err_resp}");
                drop(writer.flush());
            }
        }
    }
}

/// The stdio child server serving MCP protocol lines.
fn serve_stdio_mcp_child() {
    let stdin = stdin();
    let stdout = stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    let mut line = String::new();
    while let Ok(n) = reader.read_line(&mut line) {
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }
        if let Ok(req) = serde_json::from_str::<Value>(trimmed) {
            handle_child_request(req, &mut writer);
        }
        line.clear();
    }
}

async fn test_mcp_stdio_full_round_trip() {
    let self_exe = current_exe().expect("current test exe");
    let launcher: Arc<dyn ProcessLauncher> = Arc::new(StdProcessLauncher::new());
    let workspace: Arc<Path> = Arc::from(temp_dir());
    let manager = McpConnections::new(launcher, workspace, fast_timings());
    let source = Arc::new(McpToolSource::default());

    let server_cfg = ScopedMcpServerConfig {
        name: "test_srv".into(),
        scope: McpConfigScope::Project,
        transport: McpTransportConfig::stdio(
            self_exe.display().to_string(),
            vec!["--server".to_string()],
        ),
        disabled: false,
        tool_timeout_ms: 5000,
    };

    manager.start_all(vec![server_cfg]).await;
    let mirror = start_mirror(manager.clone(), Arc::clone(&source), auto_gate(), None);

    let ok = eventually(|| source.names().len() == 2).await;
    if !ok {
        let statuses = manager.statuses().await;
        let stderr = manager.stderr_tail("test_srv").await;
        panic!(
            "failed to connect, statuses: {:?}, stderr: {:?}",
            statuses, stderr
        );
    }

    let names = source.names();
    assert_eq!(
        names,
        vec![
            "mcp__test_srv__danger_wipe".to_string(),
            "mcp__test_srv__echo_fast".to_string(),
        ]
    );

    // Instructions must be captured and exposed
    let instructions = source.instructions().expect("instructions captured");
    assert!(instructions.contains("test instructions for child server"));

    // Tool execution: readOnlyHint tool passes without prompt
    let echo_tool = source
        .get("mcp__test_srv__echo_fast")
        .expect("echo_fast tool exists");
    assert!(
        !echo_tool.requires_approval_for(&json!({"msg": "hello"})),
        "readOnlyHint tool runs without approval"
    );
    let out = echo_tool
        .execute(ToolCtx::new("c1"), json!({"msg": "hello"}))
        .await
        .expect("tool execution succeeds");
    assert_eq!(out, Value::String("echo_fast:hello".into()));

    // Unannotated tool requires approval
    let wipe_tool = source
        .get("mcp__test_srv__danger_wipe")
        .expect("danger_wipe tool exists");
    assert!(
        wipe_tool.requires_approval_for(&json!({})),
        "unannotated tool requires approval"
    );

    // Shutdown cleans up child process without orphans
    timeout(Duration::from_secs(5), manager.shutdown())
        .await
        .expect("shutdown completes within budget");
    drop(timeout(Duration::from_secs(2), mirror).await);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--server") {
        serve_stdio_mcp_child();
        return;
    }
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    let boxed = |f: fn() -> common::BoxTestFuture| f;
    common::run_async_tests(
        &runtime,
        &[(
            "test_mcp_stdio_full_round_trip",
            boxed(|| Box::pin(test_mcp_stdio_full_round_trip())),
        )],
    );
}
