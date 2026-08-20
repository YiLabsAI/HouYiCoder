//! Concurrent permission-mode switch: the serve loop interleaves a
//! PermissionCycleMode control frame with an active run (the select!
//! inside the run/resume drive loop reads the control frame while the run
//! future is pending on the provider), cycles the gate, and ships a
//! PermissionMode response before the run completes. Integration tests (real
//! Runner + Server + serve loop + a blocking mock provider), so they live
//! in tests/.
//!
//! Frame-based: drive the serve loop (spawned) + send frames on the client
//! channel. The control frame QUEUES in the io channel; the serve select!
//! reads it when the run future is pending, so no timing sleep is needed —
//! the queue ordering is deterministic.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::StreamExt;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::provider::stream_from_response;
use houyicoder_api::tool::Tool;
use houyicoder_async::{PFut, PStream};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::{DefaultModeGate, ModeGate};
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponseEnvelope, ClientResponsePayload, RequestEnvelope, RequestId,
    ResponsePayload, ServerFrame, ServerRequestPayload,
};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::permission::PermissionMode as WireMode;
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalRequest, ContentBlock};
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, ModelCapabilities, OutputItem, ProviderError, Usage,
};
use houyicoder_service::server::Server;
use houyicoder_session::SessionStore;

use common::{pair, recv_frame_within, recv_hello, send_frame};

/// A provider that blocks on a Notify before returning, so the run future
/// stays pending while a control frame is injected.
struct BlockingProvider {
    notify: Arc<tokio::sync::Notify>,
}

impl ModelProvider for BlockingProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let notify = self.notify.clone();
        Box::pin(async move {
            notify.notified().await;
            Ok(CompletionResponse {
                output: vec![OutputItem::Text {
                    text: "done".into(),
                }],
                usage: Usage::default(),
                model: "test".into(),
            })
        })
    }
    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> PStream<'_, Result<houyicoder_protocol::llm::LlmEvent, ProviderError>> {
        let notify = self.notify.clone();
        let s = futures::stream::once(async move {
            notify.notified().await;
            let resp = CompletionResponse {
                output: vec![OutputItem::Text {
                    text: "done".into(),
                }],
                usage: Usage::default(),
                model: "test".into(),
            };
            stream_from_response(resp)
        })
        .flatten();
        Box::pin(s)
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A tool that requires approval, so the run interrupts with a reverse
/// Permission ask + resumes after the client approves — exercises the
/// resume-select! path (a different concurrent-control branch than the first run).
struct ApprovableTool;
impl Tool for ApprovableTool {
    fn name(&self) -> &str {
        "approvable"
    }
    fn description(&self) -> &str {
        "test tool requiring approval"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn execute(
        &self,
        _ctx: houyicoder_api::tool::ToolCtx,
        _input: serde_json::Value,
    ) -> PFut<'_, Result<serde_json::Value, ToolError>> {
        Box::pin(async move { Ok(serde_json::json!({"ok": true})) })
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

/// First call returns a tool call (Interruption); second call blocks on Notify
/// (so the resume select! is live when the control frame arrives).
struct TwoPhaseProvider {
    notify: Arc<tokio::sync::Notify>,
    calls: AtomicU32,
}

impl ModelProvider for TwoPhaseProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let calls = self.calls.load(Ordering::SeqCst);
        if calls == 0 {
            Box::pin(async move {
                Ok(CompletionResponse {
                    output: vec![OutputItem::ToolCall {
                        id: "toolu_1".into(),
                        name: "approvable".into(),
                        input: serde_json::json!({}),
                    }],
                    usage: Usage::default(),
                    model: "test".into(),
                })
            })
        } else {
            let notify = self.notify.clone();
            Box::pin(async move {
                notify.notified().await;
                Ok(CompletionResponse {
                    output: vec![OutputItem::Text {
                        text: "done".into(),
                    }],
                    usage: Usage::default(),
                    model: "test".into(),
                })
            })
        }
    }
    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> PStream<'_, Result<houyicoder_protocol::llm::LlmEvent, ProviderError>> {
        let calls = self.calls.fetch_add(1, Ordering::SeqCst);
        if calls == 0 {
            let resp = CompletionResponse {
                output: vec![OutputItem::ToolCall {
                    id: "toolu_1".into(),
                    name: "approvable".into(),
                    input: serde_json::json!({}),
                }],
                usage: Usage::default(),
                model: "test".into(),
            };
            stream_from_response(resp)
        } else {
            let notify = self.notify.clone();
            let s = futures::stream::once(async move {
                notify.notified().await;
                let resp = CompletionResponse {
                    output: vec![OutputItem::Text {
                        text: "done".into(),
                    }],
                    usage: Usage::default(),
                    model: "test".into(),
                };
                stream_from_response(resp)
            })
            .flatten();
            Box::pin(s)
        }
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Spawn serve + handshake. Returns the client channel halves.
async fn handshake(
    server: Server,
) -> (
    futures::channel::mpsc::Sender<String>,
    futures::channel::mpsc::Receiver<String>,
) {
    let (io, mut client_tx, mut client_rx) = pair();
    tokio::spawn(async move { server.serve(io).await });
    send_frame(&mut client_tx, &Hello::local()).await;
    let _ = recv_hello(&mut client_rx).await;
    (client_tx, client_rx)
}

/// A PermissionCycleMode control frame cycles the gate + ships a PermissionMode
/// response BEFORE the run completes. The frame queues in io.rx; the serve
/// select! reads it when the run is pending (blocked on the provider).
#[tokio::test]
async fn test_cycle_during_run() {
    let notify = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn ModelProvider> = Arc::new(BlockingProvider {
        notify: notify.clone(),
    });
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = houyicoder_context::SessionId::new();
    let wire_session_id = houyicoder_protocol::frontend::SessionId(session.to_string());
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let gate_assert = gate.clone();
    let runner = Arc::new(Runner::with_shared_store(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let (mut client_tx, mut client_rx) = handshake(Server::new(runner, session, gate)).await;

    // Start the run (blocks on the provider). Then send the concurrent mode cycle
    // frame — it queues in io.rx; the serve select! reads it while the run
    // is pending.
    let req = RequestEnvelope::new(
        RequestId(1),
        FrontendRequest::MessageSend {
            session_id: wire_session_id,
            content: vec![ContentBlock::Text { text: "go".into() }],
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;
    let req = RequestEnvelope::new(RequestId(2), FrontendRequest::PermissionCycleMode);
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    // Drain frames until the concurrent mode response arrives.
    // recv_frame panics on channel close, so the loop either hits the
    // PermissionMode (success) or panics (the server closed without it).
    loop {
        match recv_frame_within(&mut client_rx, std::time::Duration::from_secs(5)).await {
            ServerFrame::Response(env)
                if matches!(env.payload, ResponsePayload::PermissionMode(_)) =>
            {
                let ResponsePayload::PermissionMode(m) = env.payload else {
                    unreachable!()
                };
                assert_eq!(m, WireMode::Manual, "concurrent mode cycle -> Manual");
                break;
            }
            _ => continue,
        }
    }
    assert_eq!(
        gate_assert.current(),
        houyicoder_permission::PermissionMode::Manual,
        "gate reflects the concurrent switch"
    );
    notify.notify_one();
}

/// A concurrent mode cycle during RESUME (after an approval): the first run interrupts
/// with a Permission ask, the client approves, the resume blocks on the
/// provider, + a concurrent mode cycle frame is handled by the resume select!.
#[tokio::test]
async fn test_cycle_during_resume() {
    let notify = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn ModelProvider> = Arc::new(TwoPhaseProvider {
        notify: notify.clone(),
        calls: AtomicU32::new(0),
    });
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = houyicoder_context::SessionId::new();
    let wire_session_id = houyicoder_protocol::frontend::SessionId(session.to_string());
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ApprovableTool));
    let runner = Arc::new(Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let (mut client_tx, mut client_rx) = handshake(Server::new(runner, session, gate)).await;

    // Start the run; the first phase produces a tool call -> the server ships
    // a reverse Permission ask.
    let req = RequestEnvelope::new(
        RequestId(1),
        FrontendRequest::MessageSend {
            session_id: wire_session_id,
            content: vec![ContentBlock::Text { text: "go".into() }],
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    // Drain until the reverse Permission ask arrives.
    let (ask_req_id, ask_call_id) = loop {
        match recv_frame_within(&mut client_rx, std::time::Duration::from_secs(5)).await {
            ServerFrame::Request(ask)
                if matches!(ask.payload, ServerRequestPayload::Permission(_)) =>
            {
                if let ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) =
                    ask.payload
                {
                    break (ask.req_id, call_id);
                }
            }
            _ => continue,
        }
    };

    // Approve -> the server resumes (second phase blocks on the provider).
    let decision = ClientResponseEnvelope::new(
        ask_req_id,
        ClientResponsePayload::Permission(ApprovalDecision {
            call_id: ask_call_id,
            approved: true,
            updated_input: None,
            scope: "once".to_string(),
        }),
    );
    send_frame(&mut client_tx, &ClientFrame::Response(decision)).await;

    // Mode cycle during the resume select!.
    let req = RequestEnvelope::new(RequestId(2), FrontendRequest::PermissionCycleMode);
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    loop {
        match recv_frame_within(&mut client_rx, std::time::Duration::from_secs(5)).await {
            ServerFrame::Response(env)
                if matches!(env.payload, ResponsePayload::PermissionMode(_)) =>
            {
                break;
            }
            _ => continue,
        }
    }
    notify.notify_one();
}

/// Negative invariant: a NON-mode control request sent during a run is
/// silently dropped (never replied). The serve select!'s control branch
/// (handle_mode_cycle_during_run) handles ONLY PermissionCycleMode; any
/// other request read during the run returns None + the frame is consumed
/// (not re-queued), so the client never gets a reply for it. Pins the
/// "no silent failure" contract so a future change to the select! branch
/// that accidentally replies (or drops the request without consuming it)
/// is caught. Replaces the dropped inline test_mid_run_ignores_other.
#[tokio::test]
async fn test_non_mode_request_dropped() {
    let notify = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn ModelProvider> = Arc::new(BlockingProvider {
        notify: notify.clone(),
    });
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = houyicoder_context::SessionId::new();
    let wire_session_id = houyicoder_protocol::frontend::SessionId(session.to_string());
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let runner = Arc::new(Runner::with_shared_store(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let (mut client_tx, mut client_rx) = handshake(Server::new(runner, session, gate)).await;

    // Start the run (blocks on the provider). Send a non-mode control request
    // (PermissionRules — a read query) during the run.
    let req = RequestEnvelope::new(
        RequestId(1),
        FrontendRequest::MessageSend {
            session_id: wire_session_id,
            content: vec![ContentBlock::Text { text: "go".into() }],
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;
    let req = RequestEnvelope::new(RequestId(2), FrontendRequest::PermissionRules);
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    // Unblock the run + drain until the run's outcome response (req_id 1)
    // arrives, counting any reply to the PermissionRules request (req_id 2).
    notify.notify_one();
    let mut rules_replies: u32 = 0;
    loop {
        match recv_frame_within(&mut client_rx, std::time::Duration::from_secs(5)).await {
            ServerFrame::Response(env) if env.req_id == RequestId(2) => {
                rules_replies += 1;
            }
            ServerFrame::Response(env) if env.req_id == RequestId(1) => break,
            _ => continue,
        }
    }
    assert_eq!(
        rules_replies, 0,
        "the PermissionRules request sent during the run must be dropped \
         (handle_mode_cycle_during_run handles only PermissionCycleMode mid-run)"
    );
}

/// A /compact sent while a run is active is dropped, not dispatched: compact
/// rewrites the session manifest, so firing it during the parallel-tool
/// PostToolUse batch would race that batch's reference to the old manifest.
/// /compact only dispatches between runs.
#[tokio::test]
async fn test_active_run_drops_compact() {
    let notify = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn ModelProvider> = Arc::new(BlockingProvider {
        notify: notify.clone(),
    });
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = houyicoder_context::SessionId::new();
    let wire_session_id = houyicoder_protocol::frontend::SessionId(session.to_string());
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let runner = Arc::new(Runner::with_shared_store(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let (mut client_tx, mut client_rx) = handshake(Server::new(runner, session, gate)).await;

    // Start the run (blocks on the provider), then send /compact while active.
    let req = RequestEnvelope::new(
        RequestId(1),
        FrontendRequest::MessageSend {
            session_id: wire_session_id,
            content: vec![ContentBlock::Text { text: "go".into() }],
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;
    let compact_req = RequestEnvelope::new(RequestId(2), FrontendRequest::Compact);
    send_frame(&mut client_tx, &ClientFrame::Request(compact_req)).await;

    // Unblock + drain to the run's outcome (req_id 1), counting /compact replies.
    notify.notify_one();
    let mut compact_replies: u32 = 0;
    loop {
        match recv_frame_within(&mut client_rx, std::time::Duration::from_secs(5)).await {
            ServerFrame::Response(env) if env.req_id == RequestId(2) => {
                compact_replies += 1;
            }
            ServerFrame::Response(env) if env.req_id == RequestId(1) => break,
            _ => continue,
        }
    }
    assert_eq!(
        compact_replies, 0,
        "/compact sent during an active run must be dropped, not dispatched: \
         a compact firing here races the parallel-tool PostToolUse batch"
    );
}
