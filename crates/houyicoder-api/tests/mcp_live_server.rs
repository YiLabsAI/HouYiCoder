//! Real-binary live-server test for the MCP adapter. Ignored by default:
//! make check is unit tests with mock cross-layer transports (the in-memory
//! stubs in mcp.rs); a real binary spawn is an integration concern that
//! needs the local server binary present. Run with the ignored flag to
//! exercise the real-server path. The test auto-skips when no binary is
//! found, so an ignored run on a host without the binary does not fail.

#![cfg(unix)]

use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_api::mcp::McpClient;
use std::path::PathBuf;

/// Resolve the real MCP server binary for the live-server test. The env var
/// HOUYICODER_MCP_SERVER_BINARY must point at a built binary; there is no
/// hardcoded default (a local absolute path would leak a developer path and
/// only work on one machine). None when the env var is absent or the path
/// does not exist, so the test auto-skips elsewhere.
fn server_binary() -> Option<PathBuf> {
    let p = std::env::var("HOUYICODER_MCP_SERVER_BINARY").ok()?;
    let path = PathBuf::from(p);
    if path.exists() { Some(path) } else { None }
}

/// Spawn a real local MCP server, run initialize plus tools/list (in spawn),
/// then call list_projects (zero-arg, no side effect) and assert the result.
/// Proves the adapter handshake plus tool-call path against a live server
/// binary, not just the in-memory stub.
#[test]
#[ignore = "real-binary live-server test; run with the ignored flag"]
fn test_live_mcp_real_server() {
    let Some(binary) = server_binary() else {
        eprintln!(
            "skip: no real MCP server binary for the live test (set HOUYICODER_MCP_SERVER_BINARY to a server path)"
        );
        return;
    };
    let launcher = StdProcessLauncher::new();
    let (client, tools) = McpClient::spawn(binary.to_str().unwrap(), &[], &launcher)
        .expect("spawn + initialize + tools/list");
    let has_list_projects = tools.iter().any(|t| t.name() == "list_projects");
    assert!(has_list_projects, "tools/list must include list_projects");
    let result = client
        .call_tool("list_projects", serde_json::json!({}))
        .expect("tools/call list_projects");
    assert!(result.is_object(), "tools/call returns a result object");
}
