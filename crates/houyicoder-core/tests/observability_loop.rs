//! Trace-substrate integrity: the durable event log, driven end-to-end
//! through the public Runner API, must preserve the invariants the
//! observability work built across modules. Unit tests cover each module;
//! this catches cross-module composition drift — the only place that can.
//!
//! Invariants pinned (each is a regression these last rounds fixed):
//! - ToolCall ↔ ToolResult pair by call_id, in append order.
//! - One TurnUsage per logical turn, terminal (recovery=false); a
//!   length-recovery retry shares the same turn with call_in_turn stepping.
//! - No HookSignal when no hooks are configured (Allow is derivable from
//!   absence — the inverse is pinned in-crate by fire_tests with a wired
//!   hook, which the private hook API cannot reach from here).
//! - replay (trajectory_snapshot) returns events in append order.

use std::sync::Arc;
use std::sync::Mutex;

use houyicoder_api::provider::ModelProvider;
use houyicoder_api::provider::stream_from_response;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{SessionId, TurnEvent, TurnEventKind};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{RunOutcome, Runner, ToolRegistry};

/// Pops canned CompletionResponses in sequence (last repeats). stream uses
/// stream_from_response so each call's Finish is a clean stop.
struct FakeProvider {
    responses: Mutex<Option<std::vec::IntoIter<CompletionResponse>>>,
    last: Mutex<Option<CompletionResponse>>,
}

impl FakeProvider {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses: Mutex::new(Some(responses.into_iter())),
            last: Mutex::new(None),
        }
    }
    fn next(&self) -> CompletionResponse {
        let next = {
            let mut guard = self.responses.lock().expect("script mutex");
            match &mut *guard {
                Some(iter) => iter.next().unwrap_or_else(|| {
                    self.last
                        .lock()
                        .expect("last mutex")
                        .clone()
                        .expect("script has responses")
                }),
                None => self
                    .last
                    .lock()
                    .expect("last mutex")
                    .clone()
                    .expect("script has responses"),
            }
        };
        *self.last.lock().expect("last mutex") = Some(next.clone());
        next
    }
}

impl ModelProvider for FakeProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let next = self.next();
        Box::pin(async move { Ok(next) })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        stream_from_response(self.next())
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Pops canned LlmEvent sequences per stream call (last repeats). Lets a test
/// script a length-cut (Finish reason "length") then a stop, exercising the
/// resume-direct recovery loop end-to-end.
struct FakeRawProvider {
    scripts: Mutex<Vec<Vec<LlmEvent>>>,
    next: std::sync::atomic::AtomicUsize,
}

impl FakeRawProvider {
    fn new(scripts: Vec<Vec<LlmEvent>>) -> Self {
        Self {
            scripts: Mutex::new(scripts),
            next: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl ModelProvider for FakeRawProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async {
            Ok(CompletionResponse {
                output: vec![],
                usage: Usage::default(),
                model: "test".into(),
            })
        })
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let events = {
            let guard = self.scripts.lock().expect("script mutex");
            guard[idx.min(guard.len() - 1)].clone()
        };
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }
}

/// A deterministic echo tool: returns {"echo": <input>}, read-only + safe.
struct EchoTool;
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echoes its input back"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        input: serde_json::Value,
    ) -> PFut<'_, Result<serde_json::Value, ToolError>> {
        Box::pin(async move { Ok(serde_json::json!({"echo": input})) })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
}

fn runner_with(provider: Arc<dyn ModelProvider>, tools: ToolRegistry) -> Runner {
    Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
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
    )
}

/// Collect the durable event stream via the public SessionLog accessor.
fn events(runner: &Runner, session: SessionId) -> Vec<TurnEvent> {
    runner.store().trajectory_snapshot(session)
}

#[tokio::test]
async fn test_trace_substrate_pairs_calls() {
    // Turn 1: a tool call (echo). Turn 2: final text. Two logical turns, one
    // provider round-trip each. Pins: call_id pairing, TurnUsage per turn
    // (recovery=false, turn stepping, call_in_turn=1), no HookSignal (no
    // hooks => Allow absence), and replay append order.
    let responses = vec![
        CompletionResponse {
            output: vec![
                OutputItem::Text {
                    text: "let me echo".into(),
                },
                OutputItem::ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"x": 1}),
                },
            ],
            usage: Usage {
                input_tokens: 1000,
                output_tokens: 500,
                total_tokens: 1500,
                non_cached_input_tokens: 200,
                cache_read_input_tokens: 800,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
            },
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "all done".into(),
            }],
            usage: Usage {
                input_tokens: 2000,
                output_tokens: 100,
                total_tokens: 2100,
                non_cached_input_tokens: 500,
                cache_read_input_tokens: 1500,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
            },
            model: "test".into(),
        },
    ];
    let provider = Arc::new(FakeProvider::new(responses));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let runner = runner_with(provider, tools);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));

    let ev = events(&runner, session);

    // call_id causal chain: the ToolCall precedes its ToolResult, same id.
    let tool_call = ev
        .iter()
        .position(|e| matches!(&e.kind, TurnEventKind::ToolCall { call_id, .. } if call_id == "c1"))
        .expect("ToolCall c1");
    let tool_result = ev
        .iter()
        .position(
            |e| matches!(&e.kind, TurnEventKind::ToolResult { call_id, .. } if call_id == "c1"),
        )
        .expect("ToolResult c1");
    assert!(
        tool_call < tool_result,
        "ToolResult must follow its ToolCall in append order"
    );

    // One TurnUsage per logical turn, terminal (recovery=false), turn steps
    // 1→2, call_in_turn=1 (single round-trip each). Regression guard for
    // the turn/call split: a turn must NOT be one-per-call.
    let usages: Vec<&TurnEvent> = ev
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::TurnUsage { .. }))
        .collect();
    assert_eq!(usages.len(), 2, "one TurnUsage per logical turn");
    let (t1, c1, r1) = match &usages[0].kind {
        TurnEventKind::TurnUsage {
            turn,
            call_in_turn,
            recovery,
            ..
        } => (*turn, *call_in_turn, *recovery),
        _ => unreachable!(),
    };
    let (t2, c2, r2) = match &usages[1].kind {
        TurnEventKind::TurnUsage {
            turn,
            call_in_turn,
            recovery,
            ..
        } => (*turn, *call_in_turn, *recovery),
        _ => unreachable!(),
    };
    assert_eq!(
        (t1, c1, r1),
        (1, 1, false),
        "turn 1: logical turn 1, call 1, not a retry"
    );
    assert_eq!(
        (t2, c2, r2),
        (2, 1, false),
        "turn 2: logical turn 2, call 1, not a retry"
    );

    // No hooks configured => no HookSignal (Allow is absent — derivable).
    // The inverse (a wired hook lands a signal with hook_name) is pinned
    // in-crate by fire_tests, which the private hook API cannot reach here.
    let hook_signals = ev
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::HookSignal { .. }))
        .count();
    assert_eq!(hook_signals, 0, "no hooks => no HookSignal (Allow absence)");
}

#[tokio::test]
async fn test_trace_substrate_length_retry() {
    // A length-cut on call 1 fires the resume-direct recovery loop: the
    // partial + nudge append, then a re-call that stops. Pins #75/#76 at the
    // public-API seam: both round-trips record a TurnUsage, the retry shares
    // the logical turn (turn=1 for both, NOT 1/2) with call_in_turn stepping
    // 1→2, and recovery=true on the retry.
    let retry_usage = Usage {
        input_tokens: 1000,
        output_tokens: 500,
        total_tokens: 1500,
        non_cached_input_tokens: 200,
        cache_read_input_tokens: 800,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    };
    let final_usage = Usage {
        input_tokens: 4000,
        output_tokens: 600,
        total_tokens: 4600,
        non_cached_input_tokens: 500,
        cache_read_input_tokens: 3500,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    };
    let provider = Arc::new(FakeRawProvider::new(vec![
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t1".into() },
            LlmEvent::TextDelta {
                id: "t1".into(),
                text: "partial".into(),
            },
            LlmEvent::TextEnd { id: "t1".into() },
            LlmEvent::Finish {
                reason: "length".into(),
                usage: Some(retry_usage.clone()),
            },
        ],
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t2".into() },
            LlmEvent::TextDelta {
                id: "t2".into(),
                text: " done".into(),
            },
            LlmEvent::TextEnd { id: "t2".into() },
            LlmEvent::Finish {
                reason: "stop".into(),
                usage: Some(final_usage.clone()),
            },
        ],
    ]));
    let runner = runner_with(provider, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));

    let ev = events(&runner, session);
    let usages: Vec<&TurnEvent> = ev
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::TurnUsage { .. }))
        .collect();
    assert_eq!(usages.len(), 2, "retry + terminal each record a TurnUsage");
    // The retry shares the logical turn with the terminal — turn=1 for both
    // (regression guard for the turn_count split). call_in_turn steps 1→2.
    let (t_retry, c_retry, r_retry) = match &usages[0].kind {
        TurnEventKind::TurnUsage {
            turn,
            call_in_turn,
            recovery,
            ..
        } => (*turn, *call_in_turn, *recovery),
        _ => unreachable!(),
    };
    let (t_final, c_final, r_final) = match &usages[1].kind {
        TurnEventKind::TurnUsage {
            turn,
            call_in_turn,
            recovery,
            ..
        } => (*turn, *call_in_turn, *recovery),
        _ => unreachable!(),
    };
    assert_eq!(
        (t_retry, c_retry, r_retry),
        (1, 1, true),
        "retry: turn 1, call 1, recovery"
    );
    assert_eq!(
        (t_final, c_final, r_final),
        (1, 2, false),
        "terminal: turn 1, call 2, not recovery"
    );
}
