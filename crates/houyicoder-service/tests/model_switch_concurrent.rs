//! A model switch submitted while a turn is in flight. The serve loop routes
//! a concurrent ModelSet through the same apply path the between-runs
//! dispatch uses, so the switch is answered instead of dropped, and it takes
//! effect on the next request: the in-flight request already carries the old
//! model, and the reply says so rather than claiming the switch changed it.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crate::common::{self, pair, recv_hello, send_frame};
use futures::StreamExt;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::provider::stream_from_response;
use houyicoder_api::tool::Tool;
use houyicoder_async::{PFut, PStream};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::SessionId;
use houyicoder_protocol::frontend::model::EffectiveFrom;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_service::server::Server;
use houyicoder_session::SessionStore;

/// A no-approval tool the first model call asks for, so the run reaches a
/// second model request for the switch to land on.
struct PingTool;

impl Tool for PingTool {
    fn name(&self) -> &str {
        "ping"
    }

    fn description(&self) -> &str {
        "test tool"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    fn execute(
        &self,
        _ctx: houyicoder_api::tool::ToolCtx,
        _input: serde_json::Value,
    ) -> PFut<'_, Result<serde_json::Value, ToolError>> {
        Box::pin(async move { Ok(serde_json::json!({"ok": true})) })
    }
}

/// Records the model every request names. The first request is held open
/// until released, so a control frame lands while the run future is pending;
/// the first call asks for a tool so the run reaches a second request.
struct RecordingProvider {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    seen: Mutex<Vec<String>>,
    calls: std::sync::atomic::AtomicU32,
}

impl RecordingProvider {
    fn new(started: Arc<tokio::sync::Notify>, release: Arc<tokio::sync::Notify>) -> Self {
        Self {
            started,
            release,
            seen: Mutex::new(Vec::new()),
            calls: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl ModelProvider for RecordingProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async move {
            Ok(CompletionResponse {
                output: vec![OutputItem::Text {
                    text: "done".into(),
                }],
                usage: Usage::default(),
                model: "test".into(),
            })
        })
    }

    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let model = req.model.clone();
        self.seen.lock().unwrap().push(model.clone());
        let first = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let started = self.started.clone();
        let release = self.release.clone();
        let s = futures::stream::once(async move {
            if first {
                // notify_one stores a permit; notify_waiters does not, so a
                // waiter that registers after this call would wait forever.
                started.notify_one();
                release.notified().await;
            }
            let output = if first {
                vec![OutputItem::ToolCall {
                    id: "call_1".into(),
                    name: "ping".into(),
                    input: serde_json::json!({}),
                }]
            } else {
                vec![OutputItem::Text {
                    text: "done".into(),
                }]
            };
            stream_from_response(CompletionResponse {
                output,
                usage: Usage::default(),
                model,
            })
        })
        .flatten();
        Box::pin(s)
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Drain frames until the response for req_id lands, so concurrent run events
/// queued ahead of it do not mask the reply.
async fn recv_response(
    rx: &mut futures::channel::mpsc::Receiver<String>,
    req_id: u64,
) -> ResponsePayload {
    for _ in 0..64 {
        let frame = common::recv_frame_within(rx, Duration::from_secs(6)).await;
        let ServerFrame::Response(envelope) = frame else {
            continue;
        };
        if envelope.req_id == RequestId(req_id) {
            return envelope.payload;
        }
    }
    panic!("no response for request {req_id}");
}

/// A ModelSet sent while a turn is in flight is applied at the next request
/// boundary: the reply reports NextRequest with the new model, the request
/// already in flight keeps the old model, and the following request carries
/// the new one.
#[tokio::test]
async fn test_switch_uses_next_request() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = houyicoder_context::SessionId::new();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(RecordingProvider::new(started.clone(), release.clone()));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(PingTool));
    let runner = Arc::new(Runner::new(
        store,
        provider.clone(),
        tools,
        RunnerConfig {
            model: "glm-5.1".into(),
            instructions: "test".into(),
            max_turns: 2,
            ..RunnerConfig::default()
        },
    ));
    let settings =
        std::env::temp_dir().join(format!("model-concurrent-{}.json", std::process::id()));
    std::fs::write(&settings, r#"{"model":{"id":"glm-5.1"}}"#).unwrap();
    let server = Server::new(runner.clone(), session, Arc::new(DefaultModeGate::new()))
        .with_settings_path(settings.clone());
    let (io, mut client_tx, mut client_rx) = pair();
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_frame(&mut client_tx, &Hello::local()).await;
    recv_hello(&mut client_rx).await;

    send_frame(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: SessionId(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                disabled_skills: Default::default(),
            },
        )),
    )
    .await;
    // The provider reporting it has taken the request is the latch: the run is
    // in flight, holding the first request open.
    started.notified().await;

    send_frame(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(2),
            FrontendRequest::ModelSet {
                model: Some("glm-5.2".into()),
                effort: None,
                effort_toggled: false,
                speed: None,
            },
        )),
    )
    .await;
    let payload = recv_response(&mut client_rx, 2).await;
    let ResponsePayload::ModelResult(result) = payload else {
        panic!("a concurrent model set must be answered, got {payload:?}");
    };
    assert_eq!(result.applied.id, "glm-5.2", "the switch applied");
    assert_eq!(
        result.effective_from,
        EffectiveFrom::NextRequest,
        "the in-flight request cannot change under the model"
    );
    assert_eq!(
        provider.seen(),
        vec!["glm-5.1".to_string()],
        "the in-flight request keeps the model it was built with"
    );

    release.notify_one();
    let outcome = recv_response(&mut client_rx, 1).await;
    assert!(
        matches!(outcome, ResponsePayload::RunOk(_)),
        "the run completes under the new configuration, got {outcome:?}"
    );
    assert_eq!(
        provider.seen(),
        vec!["glm-5.1".to_string(), "glm-5.2".to_string()],
        "the next request carries the switched model"
    );
    assert_eq!(runner.active_model(), "glm-5.2");

    drop(client_tx);
    drop(handle.await);
    drop(std::fs::remove_file(&settings));
}
