//! Tests for the position conversion, the identifier fallback, the answer
//! rendering, and one end-to-end query over an in-memory peer.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::read;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use houyicoder_api::tool::ToolCtx;
use houyicoder_async::CancellationToken;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use houyicoder_protocol::lsp::ScopedLspServerConfig;
use serde_json::json;
use tokio::io::duplex;
use tokio::time::sleep;

use super::*;
use crate::lsp::LspTimings;
use crate::lsp::NoSpawn;
use crate::lsp::client::tests::spawn_peer;
use crate::lsp::fast_server_config;

/// A sandbox that only reads files, for tests that never run a command.
struct StubSession {
    root: Arc<Path>,
}

impl SandboxSession for StubSession {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::Unsupported("no commands in this test".into())) })
    }

    fn read_file(&self, path: &str, max_bytes: usize) -> PFut<'_, Result<Vec<u8>, SandboxError>> {
        let path = Path::new(path).to_path_buf();
        Box::pin(async move {
            let bytes = read(&path)
                .map_err(|error| SandboxError::Io(format!("{}: {error}", path.display())))?;
            Ok(bytes.into_iter().take(max_bytes).collect())
        })
    }

    fn workspace_root(&self) -> Arc<Path> {
        Arc::clone(&self.root)
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

fn config() -> ScopedLspServerConfig {
    fast_server_config("rust")
}

/// One manager over a temp workspace, with a peer answering the handshake
/// and the hover query; the sender lets a test push answers.
async fn attached(
    root: &Path,
) -> (
    Arc<LspConnections>,
    crate::lsp::client::tests::Peer,
    Arc<std::sync::Mutex<Vec<Value>>>,
) {
    let manager = Arc::new(LspConnections::new(
        Arc::new(NoSpawn),
        Arc::from(root),
        fast_timings(),
    ));
    manager.start_all(vec![config()]).await;
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
            Some("textDocument/hover") => vec![json!({"jsonrpc": "2.0", "id": id,
                "result": {"contents": {"kind": "plaintext", "value": "hover answer"}}})],
            Some("workspace/symbol") => vec![json!({"jsonrpc": "2.0", "id": id,
                "result": []})],
            Some("textDocument/definition")
            | Some("textDocument/implementation")
            | Some("textDocument/references") => {
                vec![json!({"jsonrpc": "2.0", "id": id, "result": [
                    {"uri": "file:///work/def.rs", "range": {
                        "start": {"line": 2, "character": 4},
                        "end": {"line": 2, "character": 9}}}
                ]})]
            }
            Some("textDocument/documentSymbol") => vec![json!({"jsonrpc": "2.0", "id": id,
                "result": [{"name": "probe", "kind": 6, "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 20}}}]})],
            Some("textDocument/prepareCallHierarchy") => vec![json!({"jsonrpc": "2.0", "id": id,
                "result": [{"name": "probe", "kind": 6, "uri": "file:///work/a.rs",
                    "range": {"start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 20}}}]})],
            Some("callHierarchy/incomingCalls") => vec![json!({"jsonrpc": "2.0", "id": id,
                "result": [{"from": {"name": "caller", "kind": 6, "uri": "file:///work/c.rs",
                    "range": {"start": {"line": 3, "character": 0},
                        "end": {"line": 3, "character": 9}}}}]})],
            Some("callHierarchy/outgoingCalls") => vec![json!({"jsonrpc": "2.0", "id": id,
                "result": [{"to": {"name": "callee", "kind": 6, "uri": "file:///work/d.rs",
                    "range": {"start": {"line": 4, "character": 0},
                        "end": {"line": 4, "character": 9}}}}]})],
            Some("shutdown") => vec![json!({"jsonrpc": "2.0", "id": id, "result": null})],
            _ => vec![],
        }
    };
    let (client_read, server_write) = duplex(64 * 1024);
    let (server_read, client_write) = duplex(64 * 1024);
    let peer = spawn_peer(server_read, server_write, handler);
    manager
        .attach_for_test("rust", client_read, client_write)
        .await
        .expect("attached");
    (manager, peer, seen)
}

fn tmp_root(tag: &str) -> std::path::PathBuf {
    let root = temp_dir().join(format!("houyi-lsp-tool-{tag}-{}", std::process::id()));
    drop(remove_dir_all(&root));
    create_dir_all(&root).expect("root");
    root
}

fn ctx() -> ToolCtx {
    ToolCtx::new("t")
}

/// Characters convert to UTF-16 code units: an astral glyph before the
/// caret counts twice, and a position past the line is refused.
#[test]
fn test_utf16_position() {
    let content = "let s = \"😀\"; x\nsecond";
    let position = utf16_position(content, 1, 11).expect("valid");
    assert_eq!(position.line, 0);
    assert_eq!(position.character, 11, "one astral glyph shifts by one");
    let end = utf16_position(content, 1, 14).expect("line end");
    assert_eq!(end.character, 14, "the caret at end of line maps to width");
    assert!(utf16_position(content, 1, 99).is_err());
    assert!(utf16_position(content, 9, 1).is_err());
    assert!(utf16_position(content, 0, 1).is_err());
}

/// The identifier under the caret feeds the workspace query, so an empty
/// query no longer dumps every symbol in the workspace.
#[test]
fn test_identifier_at_position() {
    let content = "fn check_widget() {}";
    assert_eq!(
        identifier_at(content, 1, 5),
        Some("check_widget".to_string())
    );
    assert_eq!(identifier_at(content, 1, 1), Some("fn".to_string()));
    assert_eq!(identifier_at("let x = 1;", 1, 8), None);
    assert_eq!(
        identifier_at("macro_name!();", 1, 11),
        Some("macro_name".to_string())
    );
}

/// The location answers parse in every shape a server uses, and the path
/// behind a URI is percent-decoded.
#[test]
fn test_locations_shapes() {
    let filter = NoiseFilter::new(&temp_dir());
    let single = json!({"uri": "file:///w/a%20b.rs", "range": {"start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 4}}});
    assert_eq!(locations_text(&single, &filter), "/w/a b.rs:3:5");
    let list = json!([single.clone(), single]);
    assert_eq!(locations_text(&list, &filter).lines().count(), 2);
    let link = json!({"targetUri": "file:///w/c.rs",
        "targetRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}});
    assert_eq!(locations_text(&link, &filter), "/w/c.rs:1:1");
    assert_eq!(locations_text(&json!(null), &filter), "");
}

/// The outline nests children under their owner.
#[test]
fn test_symbols_nest() {
    let symbols: Vec<LspDocumentSymbol> = serde_json::from_value(json!([
        {"name": "outer", "kind": 12,
         "range": {"start": {"line": 0, "character": 0}, "end": {"line": 9, "character": 0}},
         "children": [
            {"name": "inner", "kind": 6,
             "range": {"start": {"line": 2, "character": 0}, "end": {"line": 3, "character": 0}}}
         ]}
    ]))
    .expect("parses");
    let text = symbols_text(&symbols, 0);
    assert!(text.contains("outer (line 1)"), "{text}");
    assert!(text.contains("  inner (line 3)"), "{text}");
}

/// One hover query over an attached peer answers with the server's text,
/// and the target file is read through the sandbox.
#[tokio::test]
async fn test_hover_query_answers() {
    let root = tmp_root("hover");
    let file = root.join("a.rs");
    write(&file, "fn probe() {}\n").expect("file");
    let (manager, _peer, seen) = attached(&root).await;
    let tool = LspTool::new(
        Arc::clone(&manager),
        Arc::new(StubSession {
            root: Arc::from(root.as_path()),
        }),
    );
    let result = tool
        .execute(
            ctx(),
            json!({"operation": "hover", "filePath": file.to_string_lossy(),
                "line": 1, "character": 5}),
        )
        .await
        .expect("answers");
    assert!(
        result["result"]
            .as_str()
            .unwrap_or_default()
            .contains("hover answer")
    );
    let methods: Vec<String> = seen
        .lock()
        .expect("seen")
        .iter()
        .filter_map(|frame| frame.get("method").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    assert!(
        methods
            .iter()
            .any(|method| method == "textDocument/didOpen"),
        "the document was opened before the query"
    );
    assert!(
        !methods
            .iter()
            .any(|method| method == "textDocument/didSave"),
        "a read-only query never announces a save: {methods:?}"
    );
    manager.shutdown().await;
    drop(remove_dir_all(&root));
}

/// The rendered answer is cut at the character bound with a note, so a
/// broad search cannot flood the model's context.
#[test]
fn test_answer_truncates() {
    let long = "x".repeat(MAX_RESULT_CHARS + 10);
    let text = truncated_answer(&long);
    assert!(
        text.starts_with(&"x".repeat(MAX_RESULT_CHARS)),
        "the kept prefix stands"
    );
    assert!(text.contains("[truncated at"), "{text}");
    assert!(!truncated_answer("short").contains("[truncated at"));
}

/// A server that reports an indexing pass when a query arrives marks the
/// answer as possibly partial, so the model knows the search may be
/// incomplete rather than concluding the symbol does not exist.
#[tokio::test]
async fn test_indexing_note_prefix() {
    let root = tmp_root("indexing");
    let file = root.join("a.rs");
    write(&file, "fn probe() {}\n").expect("file");
    let manager = Arc::new(LspConnections::new(
        Arc::new(NoSpawn),
        Arc::from(root.as_path()),
        fast_timings(),
    ));
    manager.start_all(vec![config()]).await;
    let handler = |frame: &Value| -> Vec<Value> {
        let id = frame.get("id").cloned();
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => vec![
                json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}}),
                json!({"jsonrpc": "2.0", "method": "$/progress",
                    "params": {"token": "index", "value": {"kind": "begin", "title": "indexing"}}}),
            ],
            Some("textDocument/hover") => vec![json!({"jsonrpc": "2.0", "id": id,
                "result": {"contents": {"kind": "plaintext", "value": "partial answer"}}})],
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
    let tool = LspTool::new(
        Arc::clone(&manager),
        Arc::new(StubSession {
            root: Arc::from(root.as_path()),
        }),
    );
    let result = tool
        .execute(
            ctx(),
            json!({"operation": "hover", "filePath": file.to_string_lossy(),
                "line": 1, "character": 5}),
        )
        .await
        .expect("answers");
    let text = result["result"].as_str().unwrap_or_default();
    assert!(
        text.starts_with("[note: language server is still indexing"),
        "the answer is marked partial: {text}"
    );
    manager.shutdown().await;
    drop(remove_dir_all(&root));
}

/// A server that answers an outline with the flat symbol shape is rendered
/// as a listing rather than failing the decode: the protocol allows either
/// answer, and a capability hint is not a promise.
#[tokio::test]
async fn test_flat_outline_renders() {
    let root = tmp_root("flat");
    let file = root.join("a.rs");
    write(&file, "fn probe() {}\n").expect("file");
    let manager = Arc::new(LspConnections::new(
        Arc::new(NoSpawn),
        Arc::from(root.as_path()),
        fast_timings(),
    ));
    manager.start_all(vec![config()]).await;
    let handler = |frame: &Value| -> Vec<Value> {
        let id = frame.get("id").cloned();
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                vec![json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}})]
            }
            Some("textDocument/documentSymbol") => vec![json!({"jsonrpc": "2.0", "id": id,
                "result": [{"name": "probe", "kind": 6,
                    "location": {"uri": "file:///work/a.rs", "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 10}}}}]})],
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
    let tool = LspTool::new(
        Arc::clone(&manager),
        Arc::new(StubSession {
            root: Arc::from(root.as_path()),
        }),
    );
    let result = tool
        .execute(
            ctx(),
            json!({"operation": "documentSymbol", "filePath": file.to_string_lossy(),
                "line": 1, "character": 4}),
        )
        .await
        .expect("answers");
    let text = result["result"].as_str().unwrap_or_default();
    assert!(text.contains("probe: /work/a.rs:1:1"), "{text}");
    manager.shutdown().await;
    drop(remove_dir_all(&root));
}

/// A cancelled dispatch ends without waiting out the server's budget, and
/// the failure says it was cancelled rather than reporting an empty answer.
#[tokio::test]
async fn test_cancel_ends_query() {
    let root = tmp_root("cancel");
    let file = root.join("a.rs");
    write(&file, "fn probe() {}\n").expect("file");
    let manager = Arc::new(LspConnections::new(
        Arc::new(NoSpawn),
        Arc::from(root.as_path()),
        fast_timings(),
    ));
    manager.start_all(vec![config()]).await;
    let handler = |frame: &Value| -> Vec<Value> {
        let id = frame.get("id").cloned();
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                vec![json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}})]
            }
            // The hover is never answered, so only cancellation can end it.
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
    let tool = LspTool::new(
        Arc::clone(&manager),
        Arc::new(StubSession {
            root: Arc::from(root.as_path()),
        }),
    );
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(10)).await;
        cancel.cancel();
    });
    let error = tool
        .execute(
            ToolCtx::new("t").with_cancel(token),
            json!({"operation": "hover", "filePath": file.to_string_lossy(),
                "line": 1, "character": 5}),
        )
        .await
        .expect_err("the cancelled query ends");
    assert!(error.to_string().contains("cancelled"), "{error}");
    manager.shutdown().await;
    drop(remove_dir_all(&root));
}

/// A workspace symbol query without one uses the identifier at the
/// position, and a position on no identifier asks for the query instead.
#[tokio::test]
async fn test_workspace_symbol_query() {
    let root = tmp_root("symbol");
    let file = root.join("a.rs");
    write(&file, "fn find_me() {}\n").expect("file");
    let (manager, _peer, seen) = attached(&root).await;
    let tool = LspTool::new(
        Arc::clone(&manager),
        Arc::new(StubSession {
            root: Arc::from(root.as_path()),
        }),
    );
    drop(
        tool.execute(
            ctx(),
            json!({"operation": "workspaceSymbol", "filePath": file.to_string_lossy(),
                "line": 1, "character": 5}),
        )
        .await
        .expect("answers"),
    );
    let asked = seen.lock().expect("seen").iter().find_map(|frame| {
        (frame.get("method").and_then(Value::as_str) == Some("workspace/symbol"))
            .then(|| frame["params"]["query"].clone())
    });
    assert_eq!(asked, Some(json!("find_me")));
    let error = tool
        .execute(
            ctx(),
            json!({"operation": "workspaceSymbol", "filePath": file.to_string_lossy(),
                "line": 1, "character": 14}),
        )
        .await
        .expect_err("no identifier there");
    assert!(error.to_string().contains("pass query"), "{error}");
    manager.shutdown().await;
    drop(remove_dir_all(&root));
}

/// A network path and a missing file are refused before anything starts.
#[tokio::test]
async fn test_path_guards() {
    let root = tmp_root("guards");
    let (manager, _peer, _seen) = attached(&root).await;
    let tool = LspTool::new(
        Arc::clone(&manager),
        Arc::new(StubSession {
            root: Arc::from(root.as_path()),
        }),
    );
    let unc = tool
        .execute(
            ctx(),
            json!({"operation": "hover", "filePath": "\\\\server\\share\\a.rs",
                "line": 1, "character": 1}),
        )
        .await
        .expect_err("network path refused");
    assert!(unc.to_string().contains("network paths"), "{unc}");
    let missing = tool
        .execute(
            ctx(),
            json!({"operation": "hover", "filePath": root.join("none.rs").to_string_lossy(),
                "line": 1, "character": 1}),
        )
        .await
        .expect_err("missing file refused");
    let message = missing.to_string();
    assert!(
        message.contains("none.rs"),
        "the refusal names the file the read failed on: {message}"
    );
    manager.shutdown().await;
    drop(remove_dir_all(&root));
}

/// Every operation routes to its protocol method and renders the answer the
/// server sent: definitions as path:line:col, outlines nested, and call
/// hierarchies through the two-step flow.
#[tokio::test]
async fn test_operations_dispatch() {
    let root = tmp_root("ops");
    let file = root.join("a.rs");
    write(&file, "fn probe() {}\n").expect("file");
    let (manager, _peer, _seen) = attached(&root).await;
    let tool = LspTool::new(
        Arc::clone(&manager),
        Arc::new(StubSession {
            root: Arc::from(root.as_path()),
        }),
    );
    let base = json!({"filePath": file.to_string_lossy(), "line": 1, "character": 5});
    for (operation, expected) in [
        ("goToDefinition", "/work/def.rs:3:5"),
        ("goToImplementation", "/work/def.rs:3:5"),
        ("documentSymbol", "probe (line 1)"),
        ("prepareCallHierarchy", "probe (/work/a.rs:1:1)"),
        ("incomingCalls", "caller ← /work/c.rs:4:1"),
        ("outgoingCalls", "callee → /work/d.rs:5:1"),
        ("findReferences", "/work/def.rs:3:5"),
    ] {
        let mut input = base.clone();
        input["operation"] = json!(operation);
        let result = tool.execute(ctx(), input).await.expect("answers");
        let text = result["result"].as_str().unwrap_or_default();
        assert!(text.contains(expected), "{operation}: {text}");
    }
    manager.shutdown().await;
    drop(remove_dir_all(&root));
}
