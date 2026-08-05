//! The disconnect-orphan reconnect test: split from session_reconnect_tests
//! so that file stays under the file-size gate. Covers the real trigger path
//! for the orphan-ToolCall brick — a mid-run disconnect drops run_fut
//! without cancelling via the token, leaving a ToolCall on disk with no
//! ToolResult — and proves run()'s entry reconcile repairs it before the new
//! user input so the next provider request ships a legal role:"tool".

use super::tests::*;
use super::*;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_client::{Client, InProcTransport};
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_session::SessionStore;

use crate::composition::SessionHost;
use crate::lifecycle::SessionLeaseStore;

// A tool whose execute future never resolves until the drive loop drops it.
// Pins the run mid-flight — ToolCall already persisted, ToolResult pending —
// so a disconnect at that instant leaves the exact orphan a hard crash would.
struct BlockingTool;
impl houyicoder_api::tool::Tool for BlockingTool {
    fn name(&self) -> &str {
        "blocking"
    }
    fn description(&self) -> &str {
        "test tool: never returns"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: houyicoder_api::tool::ToolCtx,
        _input: serde_json::Value,
    ) -> houyicoder_async::PFut<
        '_,
        Result<serde_json::Value, houyicoder_protocol::extension::ToolError>,
    > {
        Box::pin(std::future::pending())
    }
}

/// A mid-run disconnect drops run_fut without cancelling via the token, so a
/// ToolCall already on disk gains no ToolResult — the orphan a hard crash
/// leaves. A fresh connection's MessageSend hits run(), whose entry
/// reconcile_tool_results appends an interrupted ToolResult before the new
/// user input, so build_request_body ships role:"tool" right after the
/// assistant turn that issued the call. This is the real trigger path
/// (a fresh connection's next_frame returns None, dropping the in-flight
/// run future), not a hand-stitched orphan log. Assert on the projection (END: model input legal): every
/// assistant turn with tool_calls must be immediately followed by ToolResult
/// items whose call_id set equals the tool_call id set.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
async fn test_disconnect_orphan_repaired() {
    use houyicoder_context::TurnEventKind;
    use houyicoder_core::agent::project_input_items;
    use houyicoder_protocol::llm::InputItem;

    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let tool_call = |id: &str| CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: id.into(),
            name: "blocking".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let final_text = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn ModelProvider> =
        Arc::new(FakeProvider::new(vec![tool_call("c1"), final_text]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BlockingTool));
    let runner = Arc::new(Runner::with_shared_store(
        store.clone(),
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 10,
            ..RunnerConfig::default()
        },
    ));
    let gate: Arc<dyn houyicoder_permission::ModeGate> = Arc::new(DefaultModeGate::new());
    let next_seq = Arc::new(AtomicU64::new(0));
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(session, runner.clone(), next_seq, gate);

    // Connection 1: send "go" → run starts → provider emits ToolCall(c1) →
    // BlockingTool executes + never returns. Poll the durable store (not the
    // wire) for the ToolCall so the test does not depend on mid-run push
    // timing; the ToolCall is appended before the tool executes.
    let (client_tx1, server_rx1) = mpsc::channel::<String>(8);
    let (server_tx1, client_rx1) = mpsc::channel::<String>(8);
    let io1 = ServerIo::new(server_tx1, server_rx1);
    let host1 = host.clone();
    let s1 = session;
    let serve1 = tokio::spawn(async move {
        drop(serve_session(host1, s1, io1).await);
    });
    let mut client1 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx1, client_rx1,
    )));
    client1.connect().await.expect("handshake 1");
    client1
        .send_request(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
            },
        )
        .await
        .expect("send message 1");
    // Wait for the ToolCall to land on disk (the run is now mid-execute on
    // BlockingTool, which never returns). Bound the wait so a broken run
    // path fails fast instead of hanging the suite.
    let mut saw_call = false;
    for _ in 0..200 {
        let evs = store.replay(session).await.expect("replay 1");
        if evs.iter().any(|e| {
            matches!(
                e.kind,
                TurnEventKind::ToolCall { ref call_id, .. } if call_id == "c1"
            )
        }) {
            saw_call = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(saw_call, "ToolCall c1 must persist before the disconnect");
    // Disconnect: drop client1 → io.next_frame() returns None → serve_session
    // returns Unavailable → run_fut dropped (NOT cancelled via token) → the
    // tool's pending future is dropped, no ToolResult is appended.
    drop(client1);
    drop(serve1.await);

    // The orphan: ToolCall(c1) on disk, no matching ToolResult.
    let evs = store.replay(session).await.expect("replay orphan");
    let has_call = evs.iter().any(|e| {
        matches!(
            e.kind,
            TurnEventKind::ToolCall { ref call_id, .. } if call_id == "c1"
        )
    });
    let has_result = evs.iter().any(|e| {
        matches!(
            e.kind,
            TurnEventKind::ToolResult { ref call_id, .. } if call_id == "c1"
        )
    });
    assert!(has_call, "orphan ToolCall c1 present after disconnect");
    assert!(
        !has_result,
        "no ToolResult for c1 — the orphan the fix targets"
    );

    // Connection 2 (reattach): send "follow-up" → serve_session dispatches
    // MessageSend → runner.run(session, "follow-up"). run()'s entry
    // reconcile_tool_results appends an interrupted ToolResult for c1 BEFORE
    // the user input, then drive_loop calls the provider (turn 2 = "done").
    let (client_tx2, server_rx2) = mpsc::channel::<String>(8);
    let (server_tx2, client_rx2) = mpsc::channel::<String>(8);
    let io2 = ServerIo::new(server_tx2, server_rx2);
    let host2 = host.clone();
    let s2 = session;
    let serve2 = tokio::spawn(async move {
        drop(serve_session(host2, s2, io2).await);
    });
    let mut client2 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx2, client_rx2,
    )));
    client2.connect().await.expect("handshake 2");
    client2
        .send_request(
            RequestId(2),
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "follow-up".to_string(),
                }],
            },
        )
        .await
        .expect("send message 2");
    // Drain frames + poll the store for the final assistant message so the
    // server's push path does not backpressure on a full channel.
    let mut done = false;
    for _ in 0..200 {
        let evs = store.replay(session).await.expect("replay 2");
        if evs.iter().any(|e| {
            matches!(
                e.kind,
                TurnEventKind::AssistantMessage { ref text, .. } if text == "done"
            )
        }) {
            done = true;
            break;
        }
        drop(client2.next_frame().await);
        tokio::task::yield_now().await;
    }
    assert!(
        done,
        "run after reattach must complete (reconcile unblocked it)"
    );
    drop(client2);
    drop(serve2.await);

    // Distinguishing assertion (END, not MEANS): project the replayed log
    // into model input and require every assistant turn with tool_calls be
    // immediately followed by ToolResult items whose call_id set equals the
    // tool_call id set. No-fix red (orphan ships as assistant(tool_calls)
    // with no role:"tool"); run-entry green.
    let evs = store.replay(session).await.expect("replay final");
    let items = project_input_items(&evs, None);
    let mut checked = 0;
    let mut i = 0;
    while i < items.len() {
        if let InputItem::Assistant { tool_calls, .. } = &items[i]
            && !tool_calls.is_empty()
        {
            checked += 1;
            let expected: std::collections::HashSet<&str> =
                tool_calls.iter().map(|c| c.id.as_str()).collect();
            let mut got = std::collections::HashSet::new();
            let mut j = i + 1;
            while j < items.len() {
                match &items[j] {
                    InputItem::ToolResult { call_id, .. } => {
                        got.insert(call_id.as_str());
                        j += 1;
                    }
                    _ => break,
                }
            }
            assert_eq!(
                got, expected,
                "assistant turn with tool_calls must be immediately followed by \
                 ToolResult items covering exactly those ids; got {got:?}",
            );
        }
        i += 1;
    }
    assert_eq!(
        checked, 1,
        "exactly one assistant turn with tool_calls (the orphan) was checked"
    );
}
