//! ContextBuilder wiring: when RunnerConfig.instructions is empty, the served
//! system prompt is the assembled one (identity + project-context walk-up +
//! tool docs + env), not a thin static string. A non-empty instructions
//! overrides the assembled prompt verbatim. The served system prompt is
//! the assembled one (identity + project-context walk-up + tool docs +
//! env), not a thin static string; the thin static default is dropped.
//!
//! Also verifies the loop uses served.messages from build(): on turn 2, the
//! input sent to the provider must contain the turn 1 events (user, assistant,
//! tool result), proving the event log is re-projected through build() each
//! turn rather than held in a stale buffer.

use std::sync::{Arc, Mutex};

use houyicoder_api::provider::ModelProvider;
use houyicoder_api::provider::stream_from_response;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{ContextBackend, EventId, SessionId, TurnEvent, TurnEventKind};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, InputItem, ModelCapabilities, OutputItem, ProviderError,
};
use houyicoder_protocol::llm::{LlmEvent, Usage};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;

use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{
    CompressPolicy, HeuristicSummarizer, RunOutcome, Runner, ToolRegistry, build_manifest,
};

/// A stub provider that records the instructions string from each request
/// before returning a canned response. Asserts which system prompt the loop
/// actually served, not just what the config held.
struct RecordingProvider {
    response: CompletionResponse,
    seen: Arc<Mutex<Vec<String>>>,
}

impl RecordingProvider {
    fn text(text: &str, seen: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            response: CompletionResponse {
                output: vec![OutputItem::Text {
                    text: text.to_string(),
                }],
                usage: Usage::default(),
                model: "test".to_string(),
            },
            seen,
        }
    }
}

impl ModelProvider for RecordingProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.seen
            .lock()
            .expect("recording mutex")
            .push(req.instructions.clone());
        let resp = self.response.clone();
        Box::pin(async move { Ok(resp) })
    }
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        self.seen
            .lock()
            .expect("recording mutex")
            .push(req.instructions.clone());
        stream_from_response(self.response.clone())
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn runner_with(instructions: &str, provider: Arc<dyn ModelProvider>) -> Runner {
    Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: instructions.into(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry {
                max_attempts: 2,
                ..Retry::default()
            },
        },
    )
}

#[tokio::test]
async fn test_empty_uses_assembled_prompt() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runner = runner_with("", Arc::new(RecordingProvider::text("done", seen.clone())));
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    let captured = seen.lock().expect("recording mutex").clone();
    assert!(
        !captured.is_empty(),
        "the loop must have called the provider"
    );
    // The assembled identity section is served when instructions is empty,
    // not the old thin default string.
    assert!(
        captured[0].contains("You are houyicoder, an AI coding assistant"),
        "expected assembled identity, got: {}",
        captured[0]
    );
    assert!(
        captured[0].contains("Tool docs"),
        "expected the tool-docs section header, got: {}",
        captured[0]
    );
}

#[tokio::test]
async fn test_nonempty_overrides_assembled() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runner = runner_with(
        "you are a test agent",
        Arc::new(RecordingProvider::text("done", seen.clone())),
    );
    let session = SessionId::new();
    let _outcome = runner.run(session, "hi".into()).await.expect("run");
    let captured = seen.lock().expect("recording mutex").clone();
    assert!(!captured.is_empty());
    // A non-empty instructions is APPENDED to the assembled prompt (not a
    // replace) so the byte-stable identity/framework prefix survives for
    // prompt-cache. Both the assembled prompt and the configured text are
    // present, separated by a blank line.
    assert!(
        captured[0].contains("You are houyicoder, an AI coding assistant"),
        "assembled system prompt kept (not replaced): {}",
        captured[0]
    );
    assert!(
        captured[0].contains("you are a test agent"),
        "configured instructions appended: {}",
        captured[0]
    );
}

/// A provider that returns scripted responses in sequence while recording the
/// input items from each request. Used to verify the loop re-projects the
/// event log through build() each turn (served.messages carries prior turns).
struct InputRecordingProvider {
    responses: Mutex<std::vec::IntoIter<CompletionResponse>>,
    seen: Arc<Mutex<Vec<Vec<InputItem>>>>,
}

impl InputRecordingProvider {
    fn new(responses: Vec<CompletionResponse>, seen: Arc<Mutex<Vec<Vec<InputItem>>>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter()),
            seen,
        }
    }
}

impl ModelProvider for InputRecordingProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.seen.lock().expect("mutex").push(req.input.clone());
        let next = self.responses.lock().expect("mutex").next();
        Box::pin(async move { Ok(next.expect("script has responses")) })
    }
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        self.seen.lock().expect("mutex").push(req.input.clone());
        let next = self.responses.lock().expect("mutex").next();
        stream_from_response(next.expect("script has responses"))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

#[tokio::test]
async fn test_loop_uses_served_messages() {
    // On turn 2, the input sent to the provider must contain the turn 1
    // events (user message + assistant with tool call + tool result). This
    // proves the loop re-projects the event log through build() each turn
    // rather than holding a stale in-memory buffer.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let responses = vec![
        CompletionResponse {
            output: vec![
                OutputItem::Text {
                    text: "let me check".into(),
                },
                OutputItem::ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"x": 1}),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let provider = Arc::new(InputRecordingProvider::new(responses, seen.clone()));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(houyicoder_core::agent::StubTool::new("echo")));
    let runner = Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry {
                max_attempts: 2,
                ..Retry::default()
            },
        },
    );
    let session = SessionId::new();
    let result = runner
        .run(session, "do the task".into())
        .await
        .expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(t) if t == "done"));

    let captured = seen.lock().expect("mutex").clone();
    assert_eq!(captured.len(), 2, "two turns called the provider");

    // Turn 1: just the user message (no history yet).
    assert_eq!(captured[0].len(), 1);
    assert!(matches!(&captured[0][0], InputItem::User { content } if content == "do the task"));

    // Turn 2: user + assistant (with tool call) + tool result — proving
    // the event log was re-projected through build().
    assert!(
        captured[1].len() >= 3,
        "turn 2 must carry turn 1 events, got {} items",
        captured[1].len()
    );
    assert!(matches!(&captured[1][0], InputItem::User { content } if content == "do the task"));
    match &captured[1][1] {
        InputItem::Assistant {
            content,
            tool_calls,
        } => {
            assert_eq!(content, "let me check");
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].name, "echo");
        }
        _ => panic!("expected assistant item with tool call"),
    }
    assert!(matches!(&captured[1][2], InputItem::ToolResult { call_id, .. } if call_id == "c1"));
}

/// E2E: the loop serves the manifest-projected view (summary + verbatim
/// tail + Referenced output rehydrated from the CAS), not the raw event log.
/// This catches wiring bugs before the Compress stage writes checkpoints at runtime.
#[tokio::test]
async fn test_loop_applies_manifest() {
    let session = SessionId::new();
    let backend = InMemoryBackend::new();

    let events = seed_manifest_events(&backend, session).await;

    // tail_turns=1 keeps the last assistant turn verbatim; the older turns
    // are Summarized (folded into the summary). A large tool result is no
    // longer Referenced at Compress — Isolate owns externalization — so the
    // round keeps its Verbatim/Summarized disposition integral.
    let policy = CompressPolicy {
        tail_turns: 1,
        preserve_recent_tokens: 0,
        large_output_bytes: 50,
    };
    let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
    let summary = manifest.summary.clone().expect("manifest has summary");
    backend.write_checkpoint(manifest).await.unwrap();

    // The runner appends a new UserInput then drives the model call — the
    // new event defaults to Verbatim (not in the plan).
    let store = std::sync::Arc::new(SessionStore::new(Box::new(backend)));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let response = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider = Arc::new(InputRecordingProvider::new(vec![response], seen.clone()));
    let runner = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry {
                max_attempts: 2,
                ..Retry::default()
            },
        },
    );

    let result = runner.run(session, "new prompt".into()).await.expect("run");
    assert!(
        matches!(result.outcome, RunOutcome::FinalOutput(t) if t == "done"),
        "run must reach final output"
    );

    let captured = seen.lock().expect("mutex").clone();
    assert_eq!(captured.len(), 1, "one model call");
    assert_manifest_served(&captured[0], &summary);
}

/// Seed the backend with initial events and return them for manifest building.
/// The fixture includes a large tool result (Referenced), a summarized
// assistant turn, and a verbatim tail assistant turn.
async fn seed_manifest_events(backend: &InMemoryBackend, session: SessionId) -> Vec<TurnEvent> {
    let big = serde_json::json!({"data": "x".repeat(200)});
    let kinds = [
        TurnEventKind::UserInput {
            text: "original task".into(),
        },
        TurnEventKind::ToolCall {
            call_id: "c1".into(),
            tool: "run".into(),
            input: serde_json::json!({}),
        },
        TurnEventKind::ToolResult {
            call_id: "c1".into(),
            output: big,
            duration_ms: 0,
        },
        TurnEventKind::AssistantMessage {
            text: "old response".into(),
            thinking: None,
        },
        TurnEventKind::AssistantMessage {
            text: "latest response".into(),
            thinking: None,
        },
    ];
    let mut events = Vec::new();
    for kind in kinds {
        let ev = TurnEvent {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind,
        };
        backend.append(ev.clone()).await.unwrap();
        events.push(ev);
    }
    events
}

/// Assert the served input carries the manifest-projected view: (a) the
/// summary is present, not the raw Summarized user text; (b) the verbatim
/// tail is present; (c) the Summarized large tool result is dropped with its
/// group (not kept via a Referenced override — that is the Isolate stage's
/// job, not Compress); (d) the new UserInput (Verbatim default) is present.
fn assert_manifest_served(input: &[InputItem], summary: &str) {
    let has_summary = input
        .iter()
        .any(|i| matches!(i, InputItem::User { content } if content == summary));
    assert!(has_summary, "summary must be in the served input");

    let has_raw = input
        .iter()
        .any(|i| matches!(i, InputItem::User { content } if content == "original task"));
    assert!(!has_raw, "raw Summarized user text must not be served");

    let has_tail = input
        .iter()
        .any(|i| matches!(i, InputItem::Assistant { content, .. } if content == "latest response"));
    assert!(has_tail, "verbatim tail must be in the served input");

    // The large tool result c1 sits in the Summarized span (before the
    // verbatim boundary). It is dropped with its group — Referenced is no
    // longer a Compress disposition. The Isolate stage (PostToolUse) will
    // externalize large results in the verbatim tail; that path is covered
    // separately once Isolate lands.
    let tr = input
        .iter()
        .find(|i| matches!(i, InputItem::ToolResult { call_id, .. } if call_id == "c1"));
    assert!(
        tr.is_none(),
        "summarized tool result c1 must be dropped with its group: {input:?}"
    );

    let has_new = input
        .iter()
        .any(|i| matches!(i, InputItem::User { content } if content == "new prompt"));
    assert!(has_new, "new UserInput must be in input (default Verbatim)");
}

/// The Auto cache policy places its three breakpoints on the live request the
/// provider receives (system static prefix, last tool def, latest user message).
/// Pins the policy-application wiring so a later refactor cannot drop it.
#[tokio::test]
async fn test_cache_policy_applied_live() {
    use houyicoder_protocol::cache_policy::{BreakpointKind, CacheHint, CacheTtl};
    type BpVec = Vec<houyicoder_protocol::cache_policy::CacheBreakpoint>;
    let seen: Arc<Mutex<Vec<BpVec>>> = Arc::new(Mutex::new(Vec::new()));
    struct BpProvider {
        response: CompletionResponse,
        seen: Arc<Mutex<Vec<BpVec>>>,
    }
    impl ModelProvider for BpProvider {
        fn complete(
            &self,
            req: CompletionRequest,
        ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
            self.seen
                .lock()
                .expect("bp mutex")
                .push(req.cache_breakpoints.clone());
            let resp = self.response.clone();
            Box::pin(async move { Ok(resp) })
        }
        fn stream(
            &self,
            req: CompletionRequest,
        ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
            self.seen
                .lock()
                .expect("bp mutex")
                .push(req.cache_breakpoints.clone());
            houyicoder_api::provider::stream_from_response(self.response.clone())
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }
    let provider = Arc::new(BpProvider {
        response: CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        seen: seen.clone(),
    });
    let runner = Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    let session = SessionId::new();
    let _outcome = runner.run(session, "hi".into()).await;
    let captured = seen.lock().expect("bp mutex").clone();
    assert!(!captured.is_empty(), "a request reached the provider");
    let bps = &captured[0];
    assert_eq!(bps.len(), 3, "Auto policy placed 3 breakpoints");
    assert_eq!(bps[0].kind, BreakpointKind::SystemStaticPrefix);
    assert_eq!(bps[1].kind, BreakpointKind::LastToolDef);
    assert_eq!(bps[2].kind, BreakpointKind::LatestUserMessage);
    assert!(
        bps.iter()
            .all(|bp| matches!(bp.hint, CacheHint::Ephemeral(CacheTtl::OneHour))),
        "all breakpoints ephemeral 1h"
    );
}

/// with_cache_policy(NoCachePolicy) overrides the default Auto: the live
/// request carries no breakpoints. Covers the with_cache_policy builder
/// (the setter is otherwise dead in the default-Auto path).
#[tokio::test]
async fn test_no_cache_drops_breakpoints() {
    type BpVec = Vec<houyicoder_protocol::cache_policy::CacheBreakpoint>;
    let seen: Arc<Mutex<Vec<BpVec>>> = Arc::new(Mutex::new(Vec::new()));
    struct BpProvider {
        response: CompletionResponse,
        seen: Arc<Mutex<Vec<BpVec>>>,
    }
    impl ModelProvider for BpProvider {
        fn complete(
            &self,
            req: CompletionRequest,
        ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
            self.seen
                .lock()
                .expect("bp mutex")
                .push(req.cache_breakpoints.clone());
            let resp = self.response.clone();
            Box::pin(async move { Ok(resp) })
        }
        fn stream(
            &self,
            req: CompletionRequest,
        ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
            self.seen
                .lock()
                .expect("bp mutex")
                .push(req.cache_breakpoints.clone());
            houyicoder_api::provider::stream_from_response(self.response.clone())
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }
    let provider = Arc::new(BpProvider {
        response: CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        seen: seen.clone(),
    });
    let runner = Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    )
    .with_cache_policy(std::sync::Arc::new(
        houyicoder_api::cache_policy::NoCachePolicy,
    ));
    let session = SessionId::new();
    let _outcome = runner.run(session, "hi".into()).await;
    let captured = seen.lock().expect("bp mutex").clone();
    assert!(!captured.is_empty(), "a request reached the provider");
    assert!(
        captured[0].is_empty(),
        "NoCachePolicy placed no breakpoints: {:?}",
        captured[0]
    );
}
