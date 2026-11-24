//! Tests for this module. The call path runs against an in-memory peer, so
//! the transport and the handshake are real and nothing is spawned.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use houyicoder_api::session::SessionLog;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_context::BlockHash;
use houyicoder_context::SessionId;
use houyicoder_core::agent::ToolSource;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::{DefaultModeGate, GuardedTool, ModeGate};
use houyicoder_protocol::llm::ToolDef;
use houyicoder_protocol::mcp::{McpConfigScope, McpTransportConfig, ScopedMcpServerConfig};
use houyicoder_session::SessionStore;
use rmcp::RoleClient;
use rmcp::model::{CallToolResult, ContentBlock, Tool as DeclaredTool, ToolAnnotations};
use rmcp::service::RunningService;
use serde_json::{Map, Value, json};
use tokio::io::duplex;
use tokio::sync::mpsc::unbounded_channel;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::mcp::McpConnections;
use crate::mcp::manager::McpTimings;

use super::*;
use crate::mcp::client::HouyiMcpClientHandler;
use crate::mcp::client::serve_over;
use crate::mcp::client::tests::fake_peer;

/// The annotations a server declares. The two field names are the protocol's
/// own spelling of its behavior hints.
fn hints(read_only: Option<bool>, destructive: Option<bool>) -> ToolAnnotations {
    let mut annotations = ToolAnnotations::default();
    annotations.read_only_hint = read_only;
    annotations.destructive_hint = destructive;
    annotations
}

/// The registered name carries the namespace and both parts, and characters
/// outside the safe set become underscores so the name is addressable.
#[test]
fn test_name_namespaced_and_cleaned() {
    assert_eq!(registered_name("graph", "explore"), "mcp__graph__explore");
    assert_eq!(
        registered_name("my server", "read.file"),
        "mcp__my_server__read_file"
    );
}

/// A name past the cap keeps its head and a hash of the whole, so two long
/// names sharing a prefix stay distinct.
#[test]
fn test_name_capped_distinct() {
    let server = "s".repeat(40);
    let a = registered_name(&server, &format!("{}a", "t".repeat(40)));
    let b = registered_name(&server, &format!("{}b", "t".repeat(40)));
    assert_eq!(a.chars().count(), MAX_NAME_CHARS);
    assert_eq!(b.chars().count(), MAX_NAME_CHARS);
    assert_ne!(a, b, "the hash keeps truncated names distinct");
    assert!(a.starts_with("mcp__"));
}

/// Two tool names that differ only in a character the namespace replaces
/// normalize to one name; the hashed form keeps the second distinct.
#[test]
fn test_collision_names_distinct() {
    assert_eq!(
        registered_name("srv", "read.file"),
        registered_name("srv", "read_file"),
        "the premise: both normalize to one name"
    );
    let dot = distinguished_name("srv", "read.file");
    let underscore = distinguished_name("srv", "read_file");
    assert_ne!(dot, underscore, "the hash keeps the two reachable");
    assert!(dot.starts_with("mcp__srv__read_file"));
    assert!(dot.chars().count() <= MAX_NAME_CHARS);
}

/// Only an explicit read-only hint without a destructive one marks the tool
/// safe. Silence stays fail-closed, and an explicit destructive one wins over
/// a read-only hint that contradicts it.
#[test]
fn test_hints_fail_closed() {
    let none = interpret_hints(None);
    assert!(!none.read_only);
    assert!(none.destructive, "silence is destructive");
    let read_only = interpret_hints(Some(&hints(Some(true), None)));
    assert!(read_only.read_only);
    assert!(!read_only.destructive);
    let explicit = interpret_hints(Some(&hints(Some(true), Some(false))));
    assert!(
        explicit.read_only,
        "an explicit non-destructive hint is safe"
    );
    assert!(!explicit.destructive);
    let contradictory = interpret_hints(Some(&hints(Some(true), Some(true))));
    assert!(!contradictory.read_only, "a destructive hint wins");
    assert!(contradictory.destructive);
    let absent = interpret_hints(Some(&hints(None, None)));
    assert!(!absent.read_only, "absent hints are unknown, not safe");
    assert!(absent.destructive);
}

/// The inline bound is inclusive. A result exactly at the budget is returned
/// whole, and one byte past it is spilled.
#[test]
fn test_inline_boundary() {
    assert!(!exceeds_inline("abcd", 4));
    assert!(exceeds_inline("abcde", 4));
}

/// A tool standing in for a source's entry, with the trait's defaults.
struct NamedTool(&'static str);

impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "a probe"
    }
    fn input_schema(&self) -> Value {
        Value::Object(serde_json::Map::new())
    }
    fn execute(
        &self,
        _ctx: houyicoder_api::tool::ToolCtx,
        _input: Value,
    ) -> houyicoder_async::PFut<'_, Result<Value, houyicoder_protocol::extension::ToolError>> {
        Box::pin(async { Ok(Value::Null) })
    }
}

/// The source answers from its snapshot. Entries appear once replaced and
/// resolve by their registered name.
#[test]
fn test_source_snapshot() {
    let source = McpToolSource::default();
    assert!(source.names().is_empty());
    assert!(source.get("mcp__x__y").is_none());
    let mut tools: std::collections::BTreeMap<String, Arc<dyn Tool>> =
        std::collections::BTreeMap::new();
    tools.insert("mcp__x__y".into(), Arc::new(NamedTool("mcp__x__y")));
    source.replace(tools);
    assert_eq!(source.names(), vec!["mcp__x__y".to_string()]);
    let defs: Vec<ToolDef> = ToolSource::tool_defs(&source);
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "mcp__x__y");
    assert!(ToolSource::get(&source, "mcp__x__y").is_some());
}

/// A peer connected over an in-memory pair, and the fake server behind it.
async fn connected() -> (
    RunningService<RoleClient, HouyiMcpClientHandler>,
    JoinHandle<()>,
) {
    const BUF: usize = 8192;
    let (client_read, server_write) = duplex(BUF);
    let (server_read, client_write) = duplex(BUF);
    let (roots_seen, _roots_rx) = unbounded_channel::<String>();
    let peer_task = fake_peer(server_read, server_write, roots_seen);
    let (service, _events) = serve_over(client_read, client_write, Arc::from(Path::new("/work")))
        .await
        .expect("the handshake completes");
    (service, peer_task)
}

/// The declaration a server sends for one probe, read-only.
fn read_only_probe() -> DeclaredTool {
    DeclaredTool::new("probe", "a probe", Map::new()).with_annotations(hints(Some(true), None))
}

/// A declared tool exposes its description and schema, and a call reaches the
/// peer and returns its text as the model sees it.
#[tokio::test]
async fn test_call_returns_text() {
    let (service, peer_task) = connected().await;
    let tool = McpTool::adapt(
        "graph",
        &read_only_probe(),
        service.peer().clone(),
        Duration::from_secs(5),
        None,
    );
    assert_eq!(tool.name(), "mcp__graph__probe");
    assert_eq!(tool.description(), "a probe");
    assert!(tool.is_read_only());
    let out = tool
        .execute(ToolCtx::new("c1"), json!({"q": "x"}))
        .await
        .expect("the call succeeds");
    assert_eq!(out, Value::String("answer".to_string()));
    peer_task.abort();
    drop(service.cancel());
}

/// A read-only declaration passes the mode gate without a card, and the call
/// runs: the annotations only reach the user through the gate, so this is the
/// path a mirrored tool takes.
#[tokio::test]
async fn test_read_only_no_prompt() {
    let (service, peer_task) = connected().await;
    let tool = McpTool::adapt(
        "graph",
        &read_only_probe(),
        service.peer().clone(),
        Duration::from_secs(5),
        None,
    );
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let guarded = GuardedTool::new(Arc::new(tool), gate);
    assert!(guarded.is_read_only());
    assert!(
        !guarded.requires_approval_for(&json!({"q": "x"})),
        "a read-only external tool runs without a card in auto mode"
    );
    let out = guarded
        .execute(ToolCtx::new("c1"), json!({"q": "x"}))
        .await
        .expect("the gate allows the call and it reaches the peer");
    assert_eq!(out, Value::String("answer".to_string()));
    peer_task.abort();
    drop(service.cancel());
}

/// A tool that declares nothing reads as destructive, so the gate asks and
/// the call never reaches the server unless the user answers.
#[tokio::test]
async fn test_unannotated_call_asks() {
    let (service, peer_task) = connected().await;
    let silent = DeclaredTool::new("wipe", "wipes everything", Map::new());
    let tool = McpTool::adapt(
        "graph",
        &silent,
        service.peer().clone(),
        Duration::from_secs(5),
        None,
    );
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let guarded = GuardedTool::new(Arc::new(tool), gate);
    assert!(
        guarded.requires_approval_for(&json!({})),
        "an unannotated external tool asks before it runs"
    );
    let err = guarded
        .execute(ToolCtx::new("c1"), json!({}))
        .await
        .expect_err("the inline path refuses an unanswered ask");
    assert!(err.to_string().contains("approval"), "{err}");
    peer_task.abort();
    drop(service.cancel());
}

/// A call whose input is not an object is refused before anything reaches
/// the peer, and the refusal names the shape it received.
#[tokio::test]
async fn test_call_rejects_non_object() {
    let (service, peer_task) = connected().await;
    let tool = McpTool::adapt(
        "graph",
        &read_only_probe(),
        service.peer().clone(),
        Duration::from_secs(5),
        None,
    );
    let err = tool
        .execute(ToolCtx::new("c1"), json!("plain text"))
        .await
        .expect_err("a string is not a call input");
    assert!(err.to_string().contains("a string"), "{err}");
    peer_task.abort();
    drop(service.cancel());
}

/// A result past the inline bound is published under a block hash. The model
/// receives the marker instead of the whole text, and the marker names the
/// size and the hash.
#[tokio::test]
async fn test_large_result_spills() {
    let (service, peer_task) = connected().await;
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let log: Arc<dyn SessionLog> = store.clone();
    let tool = McpTool::adapt(
        "graph",
        &read_only_probe(),
        service.peer().clone(),
        Duration::from_secs(5),
        Some(log),
    )
    .with_inline_budget(1);
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let out = tool
        .execute(ctx, json!({}))
        .await
        .expect("the call succeeds");
    assert_eq!(
        out.get(BLOCK_BYTES_KEY).and_then(Value::as_u64),
        Some(8),
        "the marker names the stored JSON size, saw {out}"
    );
    let hash = out
        .get(BLOCK_REF_KEY)
        .and_then(Value::as_str)
        .expect("the marker names the block");
    assert_eq!(hash.len(), 64, "a content hash is a full digest");
    assert_eq!(
        out.get(PREVIEW_KEY).and_then(Value::as_str),
        Some("answer"),
        "the marker keeps the text when it fits the preview"
    );
    assert!(out.get(HINT_KEY).and_then(Value::as_str).is_some());
    peer_task.abort();
    drop(service.cancel());
}

/// The block holds the payload as JSON, so the engine's materialize path
/// reads back exactly the value the server returned rather than a preview,
/// and the marker's size is the stored length.
#[tokio::test]
async fn test_externalized_block_round_trips() {
    let (service, peer_task) = connected().await;
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let log: Arc<dyn SessionLog> = store.clone();
    let tool = McpTool::adapt(
        "graph",
        &read_only_probe(),
        service.peer().clone(),
        Duration::from_secs(5),
        Some(log),
    );
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let body = "x".repeat(PREVIEW_CHARS + 88);
    let out = tool.externalize(&ctx, Value::String(body.clone())).await;
    let hash = out
        .get(BLOCK_REF_KEY)
        .and_then(Value::as_str)
        .expect("the block is named");
    let bytes = store
        .backend()
        .block_get(&BlockHash(hash.to_string()))
        .await
        .expect("the block is readable");
    assert_eq!(
        out.get(BLOCK_BYTES_KEY).and_then(Value::as_u64),
        Some(bytes.len() as u64),
        "the marker reports the stored length"
    );
    assert_ne!(bytes.len(), PREVIEW_CHARS, "the block is not the preview");
    let decoded: Value = serde_json::from_slice(&bytes).expect("the block is a JSON document");
    assert_eq!(decoded, Value::String(body));
    assert_eq!(
        out.get(PREVIEW_KEY)
            .and_then(Value::as_str)
            .map(str::chars)
            .map(Iterator::count),
        Some(PREVIEW_CHARS)
    );
    peer_task.abort();
    drop(service.cancel());
}

/// An oversized result with no store bound still reaches the model: the
/// marker carries the preview and names why the rest is not there, rather
/// than failing a call the server answered.
#[tokio::test]
async fn test_externalize_keeps_preview() {
    let (service, peer_task) = connected().await;
    let tool = McpTool::adapt(
        "graph",
        &read_only_probe(),
        service.peer().clone(),
        Duration::from_secs(5),
        None,
    )
    .with_inline_budget(1);
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let out = tool
        .execute(ctx, json!({}))
        .await
        .expect("the result survives even though it cannot be stored");
    assert_eq!(out.get(BLOCK_BYTES_KEY).and_then(Value::as_u64), Some(8));
    assert_eq!(out.get(PREVIEW_KEY).and_then(Value::as_str), Some("answer"));
    assert!(
        out.get(BLOCK_REF_KEY).is_none(),
        "nothing was stored, so the marker names no block"
    );
    assert!(
        out.get(HINT_KEY)
            .and_then(Value::as_str)
            .is_some_and(|hint| hint.contains("no session store")),
        "the hint names why the rest is missing, saw {out}"
    );
    peer_task.abort();
    drop(service.cancel());
}

/// A null input sends the call with no arguments, which is how a server
/// tool that takes nothing is invoked.
#[tokio::test]
async fn test_call_accepts_null_input() {
    let (service, peer_task) = connected().await;
    let tool = McpTool::adapt(
        "graph",
        &read_only_probe(),
        service.peer().clone(),
        Duration::from_secs(5),
        None,
    );
    let out = tool
        .execute(ToolCtx::new("c1"), Value::Null)
        .await
        .expect("the call succeeds");
    assert_eq!(out, Value::String("answer".to_string()));
    peer_task.abort();
    drop(service.cancel());
}

/// A result the server marks as an error reaches the model as a failure,
/// carrying the text the server wrote.
#[tokio::test]
async fn test_error_result_surfaces() {
    let (service, peer_task) = connected().await;
    let tool = McpTool::adapt(
        "graph",
        &read_only_probe(),
        service.peer().clone(),
        Duration::from_secs(5),
        None,
    );
    let result = CallToolResult::error(vec![ContentBlock::text("boom")]);
    let err = tool
        .settle(ToolCtx::new("c1"), result)
        .await
        .expect_err("an error result is not a payload");
    assert!(err.to_string().contains("boom"), "{err}");
    peer_task.abort();
    drop(service.cancel());
}

/// A structured result passes through whole, and one without content and
/// without structure reads as an empty string rather than a missing value.
#[tokio::test]
async fn test_structured_result_passes() {
    let (service, peer_task) = connected().await;
    let tool = McpTool::adapt(
        "graph",
        &read_only_probe(),
        service.peer().clone(),
        Duration::from_secs(5),
        None,
    );
    let structured = json!({"rows": 2});
    let out = tool
        .settle(
            ToolCtx::new("c1"),
            CallToolResult::structured(structured.clone()),
        )
        .await
        .expect("a structured result needs no spill");
    assert_eq!(out, structured);
    let empty = tool
        .settle(ToolCtx::new("c1"), CallToolResult::success(Vec::new()))
        .await
        .expect("an empty result is still a result");
    assert_eq!(empty, Value::String(String::new()));
    peer_task.abort();
    drop(service.cancel());
}

/// Every JSON shape an input can arrive in has its own name in the refusal,
/// so a model that sent the wrong one can see what it sent.
#[test]
fn test_type_names_every_shape() {
    assert_eq!(type_name(&Value::Null), "null");
    assert_eq!(type_name(&json!(true)), "a boolean");
    assert_eq!(type_name(&json!(1)), "a number");
    assert_eq!(type_name(&json!("x")), "a string");
    assert_eq!(type_name(&json!([])), "an array");
    assert_eq!(type_name(&json!({})), "an object");
}

/// The mirror starts from an empty snapshot and ends when the manager stops,
/// so a session that shuts down leaves no task behind.
#[tokio::test]
async fn test_mirror_follows_shutdown() {
    let manager = McpConnections::new(
        Arc::new(crate::mcp::manager::tests::NoSpawn),
        Arc::from(Path::new("/work")),
        McpTimings::default(),
    );
    manager
        .start_all(vec![ScopedMcpServerConfig {
            name: "quiet".into(),
            scope: McpConfigScope::Project,
            transport: McpTransportConfig::stdio("fake", vec![]),
            disabled: true,
            tool_timeout_ms: 5_000,
        }])
        .await;
    let source = Arc::new(McpToolSource::default());
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let mirror = start_mirror(manager.clone(), Arc::clone(&source), gate, None);
    assert!(source.names().is_empty());
    manager.shutdown().await;
    timeout(Duration::from_secs(2), mirror)
        .await
        .expect("the mirror ends with the manager")
        .expect("the mirror task does not panic");
    assert!(source.names().is_empty());
}
