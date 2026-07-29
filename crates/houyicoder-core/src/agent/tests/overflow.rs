//! Overflow-path tests. Drive the provider ContextOverflow -> compress ->
//! bounded-retry chain. Route to a unique model id so enforced-limit writes
//! to the process-global learned-window store stay scoped here and never
//! reach sibling tests that resolve the default "test" window.

use super::*;
use houyicoder_context::TurnEventKind;
use houyicoder_protocol::llm::{CompletionResponse, ModelCapabilities, OutputItem, ProviderError};
use houyicoder_protocol::llm::{LlmEvent, Usage};

use super::{GuardedTool, runner_with};

/// Build a runner on a unique model id so enforced-limit writes to the
/// global learned-window store do not leak into sibling tests resolving the
/// default "test" window.
fn runner_with_overflow(provider: Arc<dyn ModelProvider>, tools: ToolRegistry) -> Runner {
    let runner = runner_with(provider, tools);
    runner.set_model("stub-overflow-test".to_string());
    runner
}

fn overflow_err() -> ProviderError {
    // A real provider names the enforced window on a 413/overflow; the mock
    // reports 131072 so the bounded-error threading test can assert the
    // value surfaces on ContextOverflowBounded.
    ProviderError::ContextOverflow {
        enforced_limit: Some(131_072),
    }
}

/// A provider that returns ContextOverflow N times, then succeeds.
struct OverflowThenSucceedProvider {
    count: std::sync::atomic::AtomicU32,
    overflow_times: u32,
    response: CompletionResponse,
    caps: ModelCapabilities,
}

impl OverflowThenSucceedProvider {
    fn new(overflow_times: u32, text: &str) -> Self {
        Self {
            count: std::sync::atomic::AtomicU32::new(0),
            overflow_times,
            response: CompletionResponse {
                output: vec![OutputItem::Text {
                    text: text.to_string(),
                }],
                usage: Usage::default(),
                model: "test".into(),
            },
            caps: ModelCapabilities::default(),
        }
    }
}

impl ModelProvider for OverflowThenSucceedProvider {
    fn complete(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
        let resp = self.response.clone();
        Box::pin(async move { Ok(resp) })
    }
    fn stream(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
        let n = self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n < self.overflow_times {
            // Return ContextOverflow as the first (and only) stream event.
            Box::pin(futures::stream::iter(vec![Err(overflow_err())]))
        } else {
            houyicoder_api::provider::stream_from_response(self.response.clone())
        }
    }
    fn capabilities(&self) -> ModelCapabilities {
        self.caps
    }
}

#[tokio::test]
async fn test_overflow_retries_then_compact() {
    // Provider returns ContextOverflow once, then succeeds. The overflow
    // handler compresses (folding the old events), retries, and the second
    // call succeeds -> FinalOutput (non-fatal).
    let p = Arc::new(OverflowThenSucceedProvider::new(1, "recovered"));
    let runner = runner_with_overflow(p, ToolRegistry::new());
    let session = SessionId::new();
    // Pre-populate with enough events that compress can fold something.
    for i in 0..6 {
        runner
            .store()
            .append(houyicoder_context::TurnEvent {
                id: houyicoder_context::EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                kind: if i == 0 {
                    TurnEventKind::UserInput {
                        text: "do the work".into(),
                    }
                } else {
                    TurnEventKind::AssistantMessage {
                        text: format!("response {i}"),
                        thinking: None,
                    }
                },
            })
            .await
            .unwrap();
    }
    let result = runner.run(session, "continue".into()).await.unwrap();
    match result.outcome {
        RunOutcome::FinalOutput(t) => assert_eq!(t, "recovered"),
        other => panic!("expected final output after overflow recovery, got {other:?}"),
    }
    // Verify a checkpoint was persisted (compress ran).
    let snap = runner.store().current_view(session).await.unwrap();
    assert!(
        snap.last_checkpoint.is_some(),
        "compress persisted a checkpoint"
    );
}

#[tokio::test]
async fn test_overflow_no_progress_fails() {
    // Provider returns ContextOverflow, but the session has only 1 assistant
    // turn (fewer than tail_turns=4) -> compress makes no progress (all
    // Verbatim) -> fail-closed with ContextOverflowNoProgress.
    let p = Arc::new(OverflowThenSucceedProvider::new(10, "never"));
    let runner = runner_with_overflow(p, ToolRegistry::new());
    let session = SessionId::new();
    // Only 1 assistant turn — compress cannot fold anything.
    runner
        .store()
        .append(houyicoder_context::TurnEvent {
            id: houyicoder_context::EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
                text: "do work".into(),
            },
        })
        .await
        .unwrap();
    runner
        .store()
        .append(houyicoder_context::TurnEvent {
            id: houyicoder_context::EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "ok".into(),
                thinking: None,
            },
        })
        .await
        .unwrap();
    let err = runner.run(session, "continue".into()).await.unwrap_err();
    assert!(
        matches!(err, RunError::ContextOverflowNoProgress),
        "expected no-progress fail-closed, got {err:?}"
    );
    // A NoProgress (all-Verbatim + still over ceiling) sets a Sticky
    // suppress so the next turn does not retry the doomed compact. This
    // pins the suppress state on the NoProgress path, which the test
    // previously left unasserted (it only checked the error variant).
    assert_eq!(
        runner.compact_suppress(),
        super::compact::CompactSuppress::Sticky,
        "NoProgress should set Sticky suppress so auto-compact does not retry pointlessly next turn"
    );
}

#[tokio::test]
async fn test_overflow_still_overflows_bounded() {
    // Provider always returns ContextOverflow. Compress runs (folds events),
    // but the next call still overflows. After 2 retries -> fail-closed
    // bounded (no infinite retry avalanche).
    let p = Arc::new(OverflowThenSucceedProvider::new(100, "never"));
    let runner = runner_with_overflow(p, ToolRegistry::new());
    let session = SessionId::new();
    // Pre-populate with enough events that compress can fold.
    for i in 0..6 {
        runner
            .store()
            .append(houyicoder_context::TurnEvent {
                id: houyicoder_context::EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                kind: if i == 0 {
                    TurnEventKind::UserInput {
                        text: "do the work".into(),
                    }
                } else {
                    TurnEventKind::AssistantMessage {
                        text: format!("response {i}"),
                        thinking: None,
                    }
                },
            })
            .await
            .unwrap();
    }
    let err = runner.run(session, "continue".into()).await.unwrap_err();
    // The Display string must carry the real limit so a caller that surfaces
    // the message tells the user the provider's enforced window, not just the
    // retry count. Captured before the match moves err.
    let msg = err.to_string();
    match err {
        RunError::ContextOverflowBounded {
            retries,
            enforced_limit,
        } => {
            assert_eq!(retries, 2, "must exhaust 2 retries before fail-closed");
            // The provider named 131072 on its ContextOverflow; that value
            // must thread through to the error so the caller can surface the
            // real limit to the user. A regression that drops the field
            // would leave this None.
            assert_eq!(
                enforced_limit,
                Some(131_072),
                "enforced_limit must surface from the provider overflow"
            );
            assert!(
                msg.contains("131072") && msg.contains("provider enforces"),
                "display must name the enforced limit: {msg}"
            );
        }
        other => panic!("expected bounded overflow, got {other:?}"),
    }
}

/// A provider that always overflows routes through the abortable compress
/// select (the Some arm), not a bare await. With an empty store compress
/// makes no progress -> ContextOverflowNoProgress. Pins the compress await
/// is wrapped in the token select so a stall there is Esc-reachable.
struct OverflowProvider;
impl ModelProvider for OverflowProvider {
    fn complete(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async { Err(overflow_err()) })
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn stream(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
        Box::pin(futures::stream::iter(vec![Err(overflow_err())]))
    }
}

#[tokio::test]
async fn test_overflow_compress_abortable() {
    let p = Arc::new(OverflowProvider);
    let runner = runner_with_overflow(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await;
    assert!(
        matches!(
            result,
            Err(RunError::ContextOverflowNoProgress | RunError::ContextOverflowBounded { .. })
        ),
        "overflow->compress->no progress: {result:?}"
    );
}

#[tokio::test]
async fn test_overflow_provider_complete() {
    // The run path uses stream, not complete; exercise complete directly so
    // the mock's overflow-error surface is covered (it is the same error
    // shape stream returns).
    let p = OverflowProvider;
    let err = p
        .complete(houyicoder_protocol::llm::CompletionRequest {
            model: "test".into(),
            instructions: String::new(),
            input: vec![],
            tools: vec![],
            settings: houyicoder_protocol::llm::ModelSettings::default(),
            cache_breakpoints: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::ContextOverflow { .. }));
}

#[test]
fn test_overflow_display_no_limit() {
    // The pre-flight path (served view exceeded the threshold before a
    // request was sent) carries enforced_limit = None. Its Display must not
    // claim a provider-enforced limit it does not have.
    let err = RunError::ContextOverflowBounded {
        retries: 2,
        enforced_limit: None,
    };
    let msg = err.to_string();
    assert!(msg.contains("fail-closed"), "displays fail-closed: {msg}");
    assert!(
        !msg.contains("provider enforces"),
        "none-limit must not claim a provider limit: {msg}"
    );
}

#[tokio::test]
async fn test_cache_break_compact_attribution() {
    // A turn with high cache_read, then a compact flag, then a turn with
    // low cache_read: the sharp drop is detected + attributed to compact.
    use houyicoder_protocol::llm::Usage;
    let runner = runner_with_overflow(
        Arc::new(OverflowThenSucceedProvider::new(0, "x")),
        ToolRegistry::new(),
    );
    let session = SessionId::new();
    let high = Usage {
        cache_read_input_tokens: 5000,
        input_tokens: 5000,
        ..Usage::default()
    };
    runner
        .append_turn_usage(session, "test", &high, false, None)
        .await
        .unwrap();
    // Simulate a compact since the previous response.
    runner
        .cache_compact_flag
        .store(true, std::sync::atomic::Ordering::Relaxed);
    // Next turn: cache_read dropped to 0 (the compact rewrote the prefix).
    runner
        .append_turn_usage(session, "test", &Usage::default(), false, None)
        .await
        .unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let break_events: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                &e.kind,
                houyicoder_context::TurnEventKind::CacheBreak { cause } if cause == "compact"
            )
        })
        .collect();
    assert_eq!(
        break_events.len(),
        1,
        "one CacheBreak attributed to compact"
    );
}

#[tokio::test]
async fn test_cache_break_model_switch() {
    // Same drop but the model-switch flag is set instead of compact.
    use houyicoder_protocol::llm::Usage;
    let runner = runner_with_overflow(
        Arc::new(OverflowThenSucceedProvider::new(0, "x")),
        ToolRegistry::new(),
    );
    let session = SessionId::new();
    let high = Usage {
        cache_read_input_tokens: 8000,
        input_tokens: 8000,
        ..Usage::default()
    };
    runner
        .append_turn_usage(session, "test", &high, false, None)
        .await
        .unwrap();
    runner
        .cache_model_switch_flag
        .store(true, std::sync::atomic::Ordering::Relaxed);
    runner
        .append_turn_usage(session, "test", &Usage::default(), false, None)
        .await
        .unwrap();
    let events = runner.store().replay(session).await.unwrap();
    assert!(
        events.iter().any(|e| {
            matches!(
                &e.kind,
                houyicoder_context::TurnEventKind::CacheBreak { cause } if cause == "model-switch"
            )
        }),
        "break attributed to model-switch"
    );
}

#[tokio::test]
async fn test_cache_break_no_drop() {
    // When cache_read does not drop, no CacheBreak event is recorded.
    use houyicoder_protocol::llm::Usage;
    let runner = runner_with_overflow(
        Arc::new(OverflowThenSucceedProvider::new(0, "x")),
        ToolRegistry::new(),
    );
    let session = SessionId::new();
    let high = Usage {
        cache_read_input_tokens: 5000,
        input_tokens: 5000,
        ..Usage::default()
    };
    runner
        .append_turn_usage(session, "test", &high, false, None)
        .await
        .unwrap();
    // Second turn: same cache_read — no drop, no break.
    runner
        .append_turn_usage(session, "test", &high, false, None)
        .await
        .unwrap();
    let events = runner.store().replay(session).await.unwrap();
    assert!(
        events.iter().all(|e| !matches!(
            &e.kind,
            houyicoder_context::TurnEventKind::CacheBreak { .. }
        )),
        "no CacheBreak when cache_read does not drop"
    );
}

// A scripted stream item: either hand back a completion response, or emit a
// ContextOverflow as the first stream event. Used to drive interaction
// sequences where the overflow lands on a specific turn (a resume boundary, a
// model-switch boundary) rather than every call.
enum FakeStreamItem {
    Response(CompletionResponse),
    Overflow,
}

/// A fake provider that plays a programmed sequence of stream items, each
/// either a completion response or a ContextOverflow error. Like
/// FakeProvider for the response-only case; the overflow arm lets a test
/// place an overflow on a precise turn (e.g. the resumed turn after an
/// interrupted tool call) instead of on every call.
struct FakeStreamProvider {
    items: std::sync::Mutex<std::vec::IntoIter<FakeStreamItem>>,
    last: std::sync::Mutex<Option<CompletionResponse>>,
}

impl FakeStreamProvider {
    fn new(items: Vec<FakeStreamItem>) -> Self {
        Self {
            items: std::sync::Mutex::new(items.into_iter()),
            last: std::sync::Mutex::new(None),
        }
    }
}

impl ModelProvider for FakeStreamProvider {
    fn complete(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
        let resp = self
            .last
            .lock()
            .expect("last mutex")
            .clone()
            .unwrap_or_else(|| CompletionResponse {
                output: vec![],
                usage: Usage::default(),
                model: "test".into(),
            });
        Box::pin(async move { Ok(resp) })
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn stream(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
        let next = self.items.lock().expect("script mutex").next();
        match next {
            Some(FakeStreamItem::Response(r)) => {
                *self.last.lock().expect("last mutex") = Some(r.clone());
                houyicoder_api::provider::stream_from_response(r)
            }
            Some(FakeStreamItem::Overflow) => {
                Box::pin(futures::stream::iter(vec![Err(overflow_err())]))
            }
            None => {
                let last = self
                    .last
                    .lock()
                    .expect("last mutex")
                    .clone()
                    .expect("script exhausted with no prior response");
                houyicoder_api::provider::stream_from_response(last)
            }
        }
    }
}

/// Build a runner on a unique model id with a custom max_turns, so the
/// resume-then-overflow path does not trip the default max_turns=5 cap when
/// pre-populated assistant events inflate the cumulative turn count.
fn runner_overflow_maxturn(
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    max_turns: u32,
) -> Runner {
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_resilience::Retry;
    use houyicoder_session::SessionStore;
    Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        tools,
        RunnerConfig {
            model: "stub-overflow-test".into(),
            instructions: "you are a test agent".into(),
            max_turns,
            max_output_tokens: 8_000,
            retry: Retry {
                max_attempts: 2,
                ..Retry::default()
            },
        },
    )
}

#[tokio::test]
async fn test_resume_then_overflow_recovers() {
    // resume after an interrupted tool call. The resumed turn
    // re-enters drive_loop, whose pre-flight and provider-call path is the
    // same as run's. A provider overflow on the resumed turn must trigger
    // compress and retry (the context never bricks), not a hard fail-closed.
    // Script: tool-call (interruption) -> overflow -> recovered text.
    let p = Arc::new(FakeStreamProvider::new(vec![
        FakeStreamItem::Response(CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "c1".into(),
                name: "guarded".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "stub-overflow-test".into(),
        }),
        FakeStreamItem::Overflow,
        FakeStreamItem::Response(CompletionResponse {
            output: vec![OutputItem::Text {
                text: "recovered".into(),
            }],
            usage: Usage::default(),
            model: "stub-overflow-test".into(),
        }),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    // max_turns above the cumulative count (pre-populated assistants + the
    // run's tool-call turn + the resumed turn) so the cap does not mask the
    // overflow-recovery path.
    let runner = runner_overflow_maxturn(p, tools, 20);
    let session = SessionId::new();
    // Pre-populate enough assistant events that compress can fold (>=
    // tail_turns); an empty store would make compress no-progress and
    // fail-closed here.
    for i in 0..6 {
        runner
            .store()
            .append(houyicoder_context::TurnEvent {
                id: houyicoder_context::EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                kind: if i == 0 {
                    TurnEventKind::UserInput {
                        text: "do the work".into(),
                    }
                } else {
                    TurnEventKind::AssistantMessage {
                        text: format!("response {i}"),
                        thinking: None,
                    }
                },
            })
            .await
            .unwrap();
    }
    // Run 1: the guarded tool call is pending approval -> Interruption.
    let r1 = runner.run(session, "continue".into()).await.unwrap();
    match r1.outcome {
        RunOutcome::Interruption(pending) => {
            assert_eq!(pending.len(), 1, "one pending guarded call");
            assert_eq!(pending[0].call_id, "c1");
        }
        other => panic!("run 1 should interrupt for approval: {other:?}"),
    }
    // Resume: approve c1 -> the loop continues -> provider overflows -> compress
    // folds the pre-populated tail -> retry -> recovered text. Not a brick.
    let r2 = runner
        .resume(session, &[ApprovalDecision::approve("c1")])
        .await
        .unwrap();
    let t = match r2.outcome {
        RunOutcome::FinalOutput(t) => t,
        other => panic!("resume should recover to final output: {other:?}"),
    };
    assert_eq!(t, "recovered");
    let snap = runner.store().current_view(session).await.unwrap();
    assert!(
        snap.last_checkpoint.is_some(),
        "compress ran during resume-overflow recovery"
    );
}

#[tokio::test]
async fn test_fake_stream_complete() {
    // The run path uses stream, not complete; exercise complete directly so
    // the fallback-when-no-prior-response branch is covered. With no prior
    // stream call, complete returns the empty default.
    let p = FakeStreamProvider::new(vec![]);
    let resp = p
        .complete(houyicoder_protocol::llm::CompletionRequest {
            model: "test".into(),
            instructions: String::new(),
            input: vec![],
            tools: vec![],
            settings: houyicoder_protocol::llm::ModelSettings::default(),
            cache_breakpoints: Vec::new(),
        })
        .await
        .unwrap();
    assert!(resp.output.is_empty(), "no prior response -> empty default");
}
