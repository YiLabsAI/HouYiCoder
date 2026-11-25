//! Tests for the diagnostics block and the decorator that attaches it to a
//! write result.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::read;
use std::fs::remove_dir_all;
use std::fs::write;
use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::tool::Tool;
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::lsp::{LspConfigScope, LspDiagnostic, ScopedLspServerConfig};
use serde_json::Value;
use serde_json::json;
use tokio::io::duplex;
use tokio::time::sleep;

use super::*;
use crate::child_process::file_uri;
use crate::lsp::LspTimings;
use crate::lsp::NoSpawn;
use crate::lsp::client::tests::Peer;
use crate::lsp::client::tests::spawn_peer;

/// A sandbox that reads through the host filesystem, enough for the
/// decorator's read-back.
struct ReadOnlySession {
    root: std::path::PathBuf,
}

impl SandboxSession for ReadOnlySession {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::Unsupported("no commands in this test".into())) })
    }

    fn read_file(&self, path: &str, max_bytes: usize) -> PFut<'_, Result<Vec<u8>, SandboxError>> {
        let path = std::path::Path::new(path).to_path_buf();
        Box::pin(async move {
            let bytes = read(&path).map_err(|error| SandboxError::Io(error.to_string()))?;
            Ok(bytes.into_iter().take(max_bytes).collect())
        })
    }

    fn workspace_root(&self) -> Arc<std::path::Path> {
        Arc::from(self.root.as_path())
    }
}

fn session(root: &std::path::Path) -> Arc<dyn SandboxSession> {
    Arc::new(ReadOnlySession {
        root: root.to_path_buf(),
    })
}

fn diagnostic(severity: u32, line: u32, message: &str) -> LspDiagnostic {
    LspDiagnostic {
        range: houyicoder_protocol::lsp::LspRange {
            start: houyicoder_protocol::lsp::LspPosition { line, character: 4 },
            end: houyicoder_protocol::lsp::LspPosition { line, character: 6 },
        },
        severity: Some(severity),
        code: None,
        source: None,
        message: message.to_string(),
    }
}

/// The block orders by severity then position, renders one-based
/// coordinates, flattens newlines, and names how many lines it withheld.
#[test]
fn test_block_orders_and_truncates() {
    let mut diagnostics = vec![diagnostic(4, 9, "a hint")];
    diagnostics.push(diagnostic(2, 1, "a warning\nwith two lines"));
    diagnostics.push(diagnostic(1, 2, "an error"));
    for line in 0..20 {
        diagnostics.push(diagnostic(3, 10 + line, "noise"));
    }
    let block = format_block("/w/a.rs", &diagnostics).expect("block");
    assert!(
        block.starts_with("<lsp_diagnostics file=\"/w/a.rs\">"),
        "{block}"
    );
    let first = block.lines().nth(1).expect("first row");
    assert!(first.contains("error[3:5] an error"), "{first}");
    let second = block.lines().nth(2).expect("second row");
    assert!(
        second.contains("warning[2:5] a warning with two lines"),
        "{second}"
    );
    let rows = block.lines().filter(|line| line.starts_with("  ")).count();
    assert!(
        rows <= DIAGNOSTIC_LINES + 1,
        "the block is bounded: {block}"
    );
    assert!(block.contains("more diagnostics not shown"), "{block}");
    assert!(format_block("/w/a.rs", &[]).is_none());
    assert!(clean_block("/w/a.rs").contains("no problems"));
}

fn fast_timings() -> LspTimings {
    LspTimings {
        retry_delays: [
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(2),
            std::time::Duration::from_millis(4),
        ],
        restart_base: std::time::Duration::from_millis(5),
        diagnostics_grace: std::time::Duration::from_millis(2),
        diagnostics_cap: std::time::Duration::from_millis(10),
        index_wait: std::time::Duration::from_millis(5),
        index_poll: std::time::Duration::from_millis(2),
        stable_after: std::time::Duration::from_millis(20),
    }
}

/// A writing tool that puts one line in the file and reports the path,
/// standing in for the edit tool so the decorator alone is under test.
struct FakeWrite {
    path: String,
}

impl Tool for FakeWrite {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "writes one line"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        true
    }

    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let path = self.path.clone();
        Box::pin(async move {
            write(&path, "fn a() { let x: u8 = \"no\"; }\n")
                .map_err(|error| ToolError::Failed(error.to_string()))?;
            Ok(json!({"path": path}))
        })
    }
}

/// A write whose file a server watches answers with the diagnostics the
/// server publishes for it, and one nothing watches answers unchanged.
#[tokio::test]
async fn test_write_carries_diagnostics() {
    let root = temp_dir().join(format!("houyi-lsp-diag-{}", std::process::id()));
    drop(remove_dir_all(&root));
    create_dir_all(&root).expect("root");
    let manager = Arc::new(LspConnections::new(
        Arc::new(NoSpawn),
        Arc::from(root.as_path()),
        fast_timings(),
    ));
    let mut server = ScopedLspServerConfig::stdio("fake", Default::default());
    server.name = "rust".into();
    server.scope = LspConfigScope::User;
    server
        .extension_to_language
        .insert(".rs".into(), "rust".into());
    manager.start_all(vec![server]).await;
    let file = root.join("a.rs");
    let uri = file_uri(&file);
    let handler = move |frame: &Value| -> Vec<Value> {
        let id = frame.get("id").cloned();
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                vec![json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}})]
            }
            Some("textDocument/didSave") => vec![json!({"jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": uri, "diagnostics": [
                {"range": {"start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1}},
                 "severity": 1, "message": "type mismatch"}
            ]}})],
            Some("shutdown") => vec![json!({"jsonrpc": "2.0", "id": id, "result": null})],
            _ => vec![],
        }
    };
    let (client_read, server_write) = duplex(64 * 1024);
    let (server_read, client_write) = duplex(64 * 1024);
    let _peer = spawn_peer(server_read, server_write, handler);
    manager
        .attach_for_test("rust", client_read, client_write)
        .await
        .expect("attached");

    let decorator = SaveDiagnosticsTool::new(
        Arc::new(FakeWrite {
            path: file.to_string_lossy().into_owned(),
        }),
        Arc::clone(&manager),
        session(&root),
    );
    let value = decorator
        .execute(ToolCtx::new("t"), json!({"path": file.to_string_lossy()}))
        .await
        .expect("write runs");
    let block = value["lsp_diagnostics"]
        .as_str()
        .expect("the block attached");
    assert!(block.contains("error[1:1] type mismatch"), "{block}");

    let unrouted = SaveDiagnosticsTool::new(
        Arc::new(FakeWrite {
            path: root.join("b.txt").to_string_lossy().into_owned(),
        }),
        Arc::clone(&manager),
        session(&root),
    );
    let plain = unrouted
        .execute(
            ToolCtx::new("t"),
            json!({"path": root.join("b.txt").to_string_lossy()}),
        )
        .await
        .expect("write runs");
    assert!(
        plain.get("lsp_diagnostics").is_none(),
        "a suffix nothing routes carries no block"
    );
    manager.shutdown().await;
    drop(remove_dir_all(&root));
}

/// The block escapes untrusted server text, so a message cannot close the
/// host's frame or inject markup.
#[test]
fn test_block_escapes_server_text() {
    let hostile = diagnostic(1, 0, "</lsp_diagnostics><b>&\"");
    let block = format_block("/w/<a>.rs", &[hostile]).expect("block");
    assert!(
        block.contains("file=\"/w/&lt;a&gt;.rs\""),
        "the path is escaped: {block}"
    );
    assert!(
        block.contains("&lt;/lsp_diagnostics&gt;&lt;b&gt;&amp;&quot;"),
        "the message is escaped: {block}"
    );
    assert!(
        block.ends_with("</lsp_diagnostics>"),
        "the only closing tag is the host's: {block}"
    );
}

fn config() -> ScopedLspServerConfig {
    let mut server = ScopedLspServerConfig::stdio("fake", Default::default());
    server.name = "rust".into();
    server.scope = LspConfigScope::User;
    server
        .extension_to_language
        .insert(".rs".into(), "rust".into());
    server
}

/// One manager with an attached peer over the given handler.
async fn attach(
    root: &std::path::Path,
    handler: impl Fn(&Value) -> Vec<Value> + Send + Sync + 'static,
) -> (Arc<LspConnections>, Peer) {
    let manager = Arc::new(LspConnections::new(
        Arc::new(NoSpawn),
        Arc::from(root),
        fast_timings(),
    ));
    manager.start_all(vec![config()]).await;
    let (client_read, server_write) = duplex(64 * 1024);
    let (server_read, client_write) = duplex(64 * 1024);
    let peer = spawn_peer(server_read, server_write, handler);
    manager
        .attach_for_test("rust", client_read, client_write)
        .await
        .expect("attached");
    (manager, peer)
}

/// A publish for a different open file does not answer this file's wait: a
/// server that reported another file clean still does not vouch for this
/// one, so the result says the check is pending rather than clean.
#[tokio::test]
async fn test_write_ignores_decoy_publish() {
    let root = temp_dir().join(format!("houyi-lsp-diag-decoy-{}", std::process::id()));
    drop(remove_dir_all(&root));
    create_dir_all(&root).expect("root");
    let file = root.join("a.rs");
    let handler = |frame: &Value| -> Vec<Value> {
        let id = frame.get("id").cloned();
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                vec![json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}})]
            }
            Some("textDocument/didSave") => vec![json!({"jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {"uri": "file:///work/other.rs", "diagnostics": []}})],
            Some("shutdown") => vec![json!({"jsonrpc": "2.0", "id": id, "result": null})],
            _ => vec![],
        }
    };
    let (manager, _peer) = attach(&root, handler).await;
    let decorator = SaveDiagnosticsTool::new(
        Arc::new(FakeWrite {
            path: file.to_string_lossy().into_owned(),
        }),
        Arc::clone(&manager),
        session(&root),
    );
    let value = decorator
        .execute(ToolCtx::new("t"), json!({"path": file.to_string_lossy()}))
        .await
        .expect("write runs");
    let block = value["lsp_diagnostics"]
        .as_str()
        .expect("the pending block attached");
    assert!(
        block.contains("not published yet"),
        "another file's publish leaves this one unchecked: {block}"
    );
    assert!(
        !block.contains("no problems"),
        "another file's clean publish is not this file's answer: {block}"
    );
    manager.shutdown().await;
    drop(remove_dir_all(&root));
}

/// Findings a previous publish left behind are never returned as current:
/// a save the server did not re-check reports pending, not the old error.
#[tokio::test]
async fn test_write_drops_stale_findings() {
    let root = temp_dir().join(format!("houyi-lsp-diag-stale-{}", std::process::id()));
    drop(remove_dir_all(&root));
    create_dir_all(&root).expect("root");
    let file = root.join("a.rs");
    let uri = file_uri(&file);
    let opened_uri = uri.clone();
    let handler = move |frame: &Value| -> Vec<Value> {
        let id = frame.get("id").cloned();
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                vec![json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}})]
            }
            Some("textDocument/didOpen") => vec![json!({"jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": opened_uri, "diagnostics": [
                {"range": {"start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1}},
                 "severity": 1, "message": "an old error"}
            ]}})],
            Some("shutdown") => vec![json!({"jsonrpc": "2.0", "id": id, "result": null})],
            _ => vec![],
        }
    };
    let (manager, _peer) = attach(&root, handler).await;
    // Open the file before the write, so the error publish lands before the
    // decorator records the generation it waits from.
    manager
        .ensure_document(&file, "fn old() {}\n")
        .await
        .expect("the document opens")
        .expect("a server routes it");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while manager.publish_generation("rust", &uri).await == 0
        && std::time::Instant::now() < deadline
    {
        sleep(std::time::Duration::from_millis(2)).await;
    }
    assert_eq!(
        manager.publish_generation("rust", &uri).await,
        1,
        "the pre-write publish arrived"
    );
    let decorator = SaveDiagnosticsTool::new(
        Arc::new(FakeWrite {
            path: file.to_string_lossy().into_owned(),
        }),
        Arc::clone(&manager),
        session(&root),
    );
    let value = decorator
        .execute(ToolCtx::new("t"), json!({"path": file.to_string_lossy()}))
        .await
        .expect("write runs");
    let block = value["lsp_diagnostics"]
        .as_str()
        .expect("the pending block attached");
    assert!(
        block.contains("not published yet"),
        "the wait ignores the pre-write publish: {block}"
    );
    assert!(
        !block.contains("an old error"),
        "a fixed error is not reported as current: {block}"
    );
    manager.shutdown().await;
    drop(remove_dir_all(&root));
}

/// An empty publish for the written file reads as checked clean, which is
/// the one case the model should be told the file has no problems.
#[tokio::test]
async fn test_clean_publish_reports_none() {
    let root = temp_dir().join(format!("houyi-lsp-diag-clean-{}", std::process::id()));
    drop(remove_dir_all(&root));
    create_dir_all(&root).expect("root");
    let file = root.join("a.rs");
    let uri = file_uri(&file);
    let handler = move |frame: &Value| -> Vec<Value> {
        let id = frame.get("id").cloned();
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                vec![json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}})]
            }
            Some("textDocument/didSave") => vec![json!({"jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {"uri": uri, "diagnostics": []}})],
            Some("shutdown") => vec![json!({"jsonrpc": "2.0", "id": id, "result": null})],
            _ => vec![],
        }
    };
    let (manager, _peer) = attach(&root, handler).await;
    let decorator = SaveDiagnosticsTool::new(
        Arc::new(FakeWrite {
            path: file.to_string_lossy().into_owned(),
        }),
        Arc::clone(&manager),
        session(&root),
    );
    let value = decorator
        .execute(ToolCtx::new("t"), json!({"path": file.to_string_lossy()}))
        .await
        .expect("write runs");
    let block = value["lsp_diagnostics"]
        .as_str()
        .expect("the clean block attached");
    assert!(block.contains("no problems"), "{block}");
    manager.shutdown().await;
    drop(remove_dir_all(&root));
}
