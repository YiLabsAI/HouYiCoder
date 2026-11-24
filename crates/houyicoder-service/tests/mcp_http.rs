//! Cross-layer tests for the remote transport. A loopback listener answers
//! the handshake with an authentication challenge, so the client path and the
//! manager's state mapping run as they do against a real gateway.

use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::thread::spawn;
use std::time::Duration;
use std::time::Instant;

use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_protocol::mcp::{
    McpConfigScope, McpConnectionState, McpTransportConfig, ScopedMcpServerConfig,
};
use houyicoder_service::mcp::{McpConnections, McpTimings, RestartPolicy};
use tokio::time::sleep;

fn fast_timings() -> McpTimings {
    McpTimings {
        notice_window: Duration::from_millis(20),
        handshake_timeout: Duration::from_secs(2),
        liveness_poll: Duration::from_millis(50),
        stable_after: Duration::from_millis(20),
        close_timeout: Duration::from_millis(200),
        restart: RestartPolicy::new(Duration::from_millis(1), Duration::from_millis(2), 2),
    }
}

/// A listener that answers every request with the same authentication
/// challenge.
fn challenge_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let address = listener.local_addr().expect("the bound address");
    spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buffer = [0u8; 4096];
            drop(stream.read(&mut buffer));
            let body = "unauthorized";
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"graph\"\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            drop(stream.write_all(response.as_bytes()));
            drop(stream.flush());
        }
    });
    format!("http://{address}/mcp")
}

async fn wait_auth(manager: &McpConnections, deadline: Duration) -> Option<String> {
    let started = Instant::now();
    while started.elapsed() < deadline {
        for status in manager.statuses().await {
            if let McpConnectionState::NeedsAuth { reason } = status.state {
                return Some(reason);
            }
        }
        sleep(Duration::from_millis(5)).await;
    }
    None
}

async fn wait_failed(manager: &McpConnections, deadline: Duration) -> Option<String> {
    let started = Instant::now();
    while started.elapsed() < deadline {
        for status in manager.statuses().await {
            if let McpConnectionState::Failed { error } = status.state {
                return Some(error);
            }
        }
        sleep(Duration::from_millis(5)).await;
    }
    None
}

fn remote_manager() -> Arc<McpConnections> {
    Arc::new(McpConnections::new(
        Arc::new(StdProcessLauncher::new()),
        Arc::from(Path::new("/work")),
        fast_timings(),
    ))
}

fn remote_config(endpoint: &str) -> ScopedMcpServerConfig {
    ScopedMcpServerConfig {
        name: "remote".into(),
        scope: McpConfigScope::User,
        transport: McpTransportConfig::http(endpoint),
        disabled: false,
        tool_timeout_ms: 5_000,
    }
}

/// A server that answers with 401 and a challenge is reported as needing
/// credentials, and the reason names the challenge rather than a transport
/// failure.
#[tokio::test]
async fn test_http_auth_challenge_reported() {
    let endpoint = challenge_server();
    let manager = remote_manager();
    manager.start_all(vec![remote_config(&endpoint)]).await;
    let reason = wait_auth(&manager, Duration::from_secs(10))
        .await
        .expect("the challenge reaches the status view");
    assert!(
        reason.contains("Bearer"),
        "the reason names the scheme the server asked for, saw {reason}"
    );
    manager.shutdown().await;
}

/// An endpoint that refuses the connection is reported as failed rather than
/// waiting forever.
#[tokio::test]
async fn test_refused_endpoint_fails() {
    let manager = remote_manager();
    manager
        .start_all(vec![remote_config("http://127.0.0.1:1/mcp")])
        .await;
    let error = wait_failed(&manager, Duration::from_secs(10))
        .await
        .expect("the refused endpoint reaches the status view");
    assert!(
        error.contains("did not start") || error.contains("error") || error.contains("connect"),
        "the failure names its cause, saw {error}"
    );
    manager.shutdown().await;
}
