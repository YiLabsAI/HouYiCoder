use super::*;
use crate::agent::runner_config::DEFAULT_SNAPSHOT_TTL_SECS;
use crate::provider::test_support::FakeProvider;
use futures::StreamExt;
use houyicoder_api::live::LiveEvent;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_context::TurnEventKind;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::llm::{CompletionRequest, CompletionResponse, OutputItem, ProviderError};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;
#[cfg(test)]
mod mid_turn;

#[cfg(test)]
mod queue_lifecycle;

#[cfg(test)]
mod isolate;

#[cfg(test)]
mod compress;

#[cfg(test)]
mod overflow;

// length is pub(super): turn_usage_emit_tests (a sibling in agent) reuses
// ScriptRawProvider from it. The other six submodules are private to tests
// (no cross-module references); length is the only one with an external user.
#[cfg(test)]
pub(super) mod length;

#[cfg(test)]
mod memory_gates;

pub(crate) fn runner_with(provider: Arc<dyn ModelProvider>, tools: ToolRegistry) -> Runner {
    Runner::new(
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
    )
}

#[tokio::test]
async fn test_run_final_no_tools() {
    let p = Arc::new(FakeProvider::text("done"));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    let text = match result.outcome {
        RunOutcome::FinalOutput(t) => t,
        _ => panic!("expected final output, got {:?}", result.outcome),
    };
    assert_eq!(text, "done");
    assert_eq!(result.turns, 1);
}

#[tokio::test]
async fn test_run_tool_then_final() {
    // A canned response with one tool call, then a final text. FakeProvider
    // returns the SAME response every call, so this loop would call the
    // tool every turn and never stop — unless max_turns caps it. Use a
    // script provider instead.
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
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "all done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let p = Arc::new(FakeProvider::new(responses));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(StubTool::new("echo")));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    match result.outcome {
        RunOutcome::FinalOutput(t) => assert_eq!(t, "all done"),
        _ => panic!("expected final output"),
    }
    assert_eq!(result.turns, 2);
}

#[tokio::test]
async fn test_run_unknown_tool_error() {
    let resp = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "no_such_tool".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![
        resp,
        CompletionResponse {
            output: vec![OutputItem::Text { text: "ok".into() }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
}

#[tokio::test]
async fn test_empty_turn_runs_again() {
    // A turn with only Reasoning (no text, no tools) must NOT end the run
    // as FinalOutput(""); it should run_again so the model gets another
    // chance. max_turns is the backstop.
    let responses = vec![
        CompletionResponse {
            output: vec![OutputItem::Reasoning {
                text: "thinking".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "now I answer".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let p = Arc::new(FakeProvider::new(responses));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    match result.outcome {
        RunOutcome::FinalOutput(t) => assert_eq!(t, "now I answer"),
        other => panic!("expected final output, got {other:?}"),
    }
    assert_eq!(result.turns, 2);
}

#[tokio::test]
async fn test_resume_cumulative_max_turns() {
    // An approval-requiring tool forces an Interruption every turn. With
    // max_turns=1, run() does 1 turn → Interruption; resume() must carry
    // the turn count (1) so the next iteration hits 2 > 1 → MaxTurnsReached.
    // If resume reset the counter to 0, the cap would not be cumulative.
    let resp = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "guarded".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let runner = Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        p,
        tools,
        RunnerConfig {
            max_turns: 1,
            ..runner_with_cfg0()
        },
    );
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    let approvals = match result.outcome {
        RunOutcome::Interruption(a) => a,
        other => panic!("expected interruption, got {other:?}"),
    };
    assert_eq!(result.turns, 1);
    let decisions: Vec<ApprovalDecision> = approvals
        .iter()
        .map(|a| ApprovalDecision::approve(&a.call_id))
        .collect();
    let result = runner.resume(session, &decisions).await.unwrap();
    assert!(matches!(
        result.outcome,
        RunOutcome::MaxTurnsReached { turns } if turns == 1
    ));
    assert_eq!(result.turns, 1);
}

#[tokio::test]
async fn test_resume_appends_rejection() {
    // Rejected approval → a rejection-note ToolResult, then the loop runs
    // again and the model sees the veto.
    let responses = vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "c1".into(),
                name: "guarded".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "ok you said no".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let p = Arc::new(FakeProvider::new(responses));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    let approvals = match result.outcome {
        RunOutcome::Interruption(a) => a,
        _ => panic!("expected interruption"),
    };
    let decisions: Vec<ApprovalDecision> = approvals
        .iter()
        .map(|a| ApprovalDecision::reject(&a.call_id))
        .collect();
    let resumed = runner.resume(session, &decisions).await.unwrap();
    match resumed.outcome {
        RunOutcome::FinalOutput(t) => assert_eq!(t, "ok you said no"),
        other => panic!("expected final output, got {other:?}"),
    }
}

fn runner_with_cfg0() -> RunnerConfig {
    RunnerConfig {
        model: "test".into(),
        instructions: "you are a test agent".into(),
        max_turns: 5,
        max_output_tokens: 8_000,
        retry: Retry {
            max_attempts: 2,
            ..Retry::default()
        },
    }
}

#[tokio::test]
async fn test_stream_persists_deltas() {
    // FakeProvider streams "hello world" as 4-char deltas. The live sink must
    // receive each delta, the session log carries only the authoritative
    // AssistantMessage (deltas are transport-only, copy + live sink, not
    // durable), and projection folds the one AssistantMessage into a single
    // assistant InputItem (no duplication).
    let p = Arc::new(FakeProvider::text("hello world"));
    let store = std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let collected: Arc<std::sync::Mutex<Vec<LiveEvent>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink: Arc<dyn Fn(&LiveEvent) + Send + Sync> = {
        let c = collected.clone();
        Arc::new(move |ev: &LiveEvent| {
            c.lock().expect("sink").push(ev.clone());
        })
    };
    let mut runner = Runner::new(
        store,
        p,
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
    runner.set_live_sink(sink);
    let result = runner.run(session, "hi".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(t) if t == "hello world"));

    // 4-char chunks of "hello world": "hell", "o wo", "rld" → 3 live deltas.
    let live = collected.lock().expect("sink").clone();
    let deltas: Vec<String> = live
        .into_iter()
        .filter_map(|ev| match ev {
            LiveEvent::AssistantDelta { text } | LiveEvent::ReasoningDelta { text } => Some(text),
            // MemorySaved is orthogonal (no extractor/dream wired); ToolProgress
            // is the bash-elapsed tick (no long bash here); SystemLine is a
            // runtime notice (no overflow here). None is a delta.
            LiveEvent::MemorySaved { .. }
            | LiveEvent::ToolProgress { .. }
            | LiveEvent::SystemLine { .. } => None,
        })
        .collect();
    assert_eq!(deltas.concat(), "hello world");
    assert_eq!(deltas.len(), 3);

    // The durable log carries only the authoritative AssistantMessage;
    // deltas are process-local transport records (copy + live sink only),
    // never persisted to the backend log, so the model-input history sees
    // one assistant message not fragments. The live sink above received all
    // 3 deltas; replay() carries zero.
    let events = runner.store().replay(session).await.expect("replay");
    let delta_n = events
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::AssistantTextDelta { .. }))
        .count();
    assert_eq!(
        delta_n, 0,
        "deltas are transport-only, not in the durable log"
    );
    let msg_n = events
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::AssistantMessage { .. }))
        .count();
    assert_eq!(msg_n, 1, "one authoritative AssistantMessage");
    let items = project_input_items(&events, None);
    let assistant = items
        .iter()
        .filter(|i| matches!(i, houyicoder_protocol::llm::InputItem::Assistant { .. }))
        .count();
    assert_eq!(assistant, 1, "projection folds deltas into one Assistant");
}

#[tokio::test]
async fn test_resume_partial_interrupts() {
    // Two approval-requiring tool calls in one turn. The caller passes a
    // decision for only the first: resume must apply that one, then re-interrupt
    // with the still-pending second call. The decided call's ToolResult is
    // appended; the undecided call stays answerable on a later resume.
    let resp = CompletionResponse {
        output: vec![
            OutputItem::ToolCall {
                id: "c1".into(),
                name: "guarded".into(),
                input: serde_json::json!({"n": 1}),
            },
            OutputItem::ToolCall {
                id: "c2".into(),
                name: "guarded".into(),
                input: serde_json::json!({"n": 2}),
            },
        ],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    let approvals = match result.outcome {
        RunOutcome::Interruption(a) => a,
        other => panic!("expected interruption, got {other:?}"),
    };
    assert_eq!(approvals.len(), 2);

    // Decide only the first call.
    let resumed = runner
        .resume(session, &[ApprovalDecision::approve("c1")])
        .await
        .unwrap();
    let remaining = match resumed.outcome {
        RunOutcome::Interruption(r) => r,
        other => panic!("expected re-interruption, got {other:?}"),
    };
    assert_eq!(remaining.len(), 1, "one call still undecided");
    assert_eq!(remaining[0].call_id, "c2");

    // The first call must have its ToolResult now; the second must not.
    let events = runner.store().replay(session).await.unwrap();
    let has_result = |cid: &str| {
        events.iter().any(|e| {
            matches!(
                &e.kind,
                TurnEventKind::ToolResult { call_id, .. } if call_id == cid
            )
        })
    };
    assert!(has_result("c1"), "decided call got a result");
    assert!(!has_result("c2"), "undecided call left pending");
}

#[tokio::test]
async fn test_resume_full_then_continues() {
    // Two approval-requiring calls; the caller decides both (one approved, one
    // rejected). No remaining pending → the loop continues to the next turn,
    // which here is a final-text response. This guards the backward-compatible
    // path: full decision set ⇒ no re-interrupt.
    let responses = vec![
        CompletionResponse {
            output: vec![
                OutputItem::ToolCall {
                    id: "c1".into(),
                    name: "guarded".into(),
                    input: serde_json::json!({"n": 1}),
                },
                OutputItem::ToolCall {
                    id: "c2".into(),
                    name: "guarded".into(),
                    input: serde_json::json!({"n": 2}),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "after approvals".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let p = Arc::new(FakeProvider::new(responses));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    let approvals = match result.outcome {
        RunOutcome::Interruption(a) => a,
        _ => panic!("expected interruption"),
    };
    assert_eq!(approvals.len(), 2);
    let decisions = vec![
        ApprovalDecision::approve("c1"),
        ApprovalDecision::reject("c2"),
    ];
    let resumed = runner.resume(session, &decisions).await.unwrap();
    match resumed.outcome {
        RunOutcome::FinalOutput(t) => assert_eq!(t, "after approvals"),
        other => panic!("expected final output, got {other:?}"),
    }
    // Verify the reject branch wrote the rejection-note result for c2.
    let events = runner.store().replay(session).await.unwrap();
    let c2_outcome = events.iter().find_map(|e| match &e.kind {
        TurnEventKind::ToolResult {
            call_id, output, ..
        } if call_id == "c2" => Some(output.clone()),
        _ => None,
    });
    assert_eq!(
        c2_outcome,
        Some(serde_json::json!({"error": "rejected by user"}))
    );
}

#[tokio::test]
async fn test_resume_empty_interrupts() {
    // Empty decision slice on a pending Interruption: every pending call stays
    // pending, and resume re-interrupts with the full set. Guards the degenerate
    // end of the partial-decision spectrum (caller poked resume with nothing).
    let resp = CompletionResponse {
        output: vec![
            OutputItem::ToolCall {
                id: "c1".into(),
                name: "guarded".into(),
                input: serde_json::json!({}),
            },
            OutputItem::ToolCall {
                id: "c2".into(),
                name: "guarded".into(),
                input: serde_json::json!({}),
            },
        ],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    assert!(matches!(result.outcome, RunOutcome::Interruption(_)));
    let resumed = runner.resume(session, &[]).await.unwrap();
    let remaining = match resumed.outcome {
        RunOutcome::Interruption(r) => r,
        other => panic!("expected re-interruption, got {other:?}"),
    };
    assert_eq!(remaining.len(), 2, "no decisions ⇒ all stay pending");
    let ids: Vec<&str> = remaining.iter().map(|a| a.call_id.as_str()).collect();
    assert_eq!(ids, vec!["c1", "c2"]);
    // Nothing got a ToolResult.
    let events = runner.store().replay(session).await.unwrap();
    let results = events
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::ToolResult { .. }))
        .count();
    assert_eq!(results, 0, "no ToolResults appended for undecided calls");
}

#[tokio::test]
async fn test_resume_drains_queue() {
    // End-to-end one-at-a-time queue: two guarded calls in one turn; the caller
    // resumes with one decision at a time. First resume decides c1, re-interrupts
    // with c2; second resume decides c2, then the loop continues to final text.
    let responses = vec![
        CompletionResponse {
            output: vec![
                OutputItem::ToolCall {
                    id: "c1".into(),
                    name: "guarded".into(),
                    input: serde_json::json!({"n": 1}),
                },
                OutputItem::ToolCall {
                    id: "c2".into(),
                    name: "guarded".into(),
                    input: serde_json::json!({"n": 2}),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "all decided".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let p = Arc::new(FakeProvider::new(responses));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    let approvals = match result.outcome {
        RunOutcome::Interruption(a) => a,
        _ => panic!("expected interruption"),
    };
    assert_eq!(approvals.len(), 2);

    // First queue pop: decide c1 only.
    let resumed = runner
        .resume(session, &[ApprovalDecision::approve("c1")])
        .await
        .unwrap();
    let next = match resumed.outcome {
        RunOutcome::Interruption(r) => r,
        other => panic!("expected re-interruption, got {other:?}"),
    };
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].call_id, "c2");

    // Second queue pop: decide c2, loop continues to final text.
    let resumed = runner
        .resume(session, &[ApprovalDecision::approve("c2")])
        .await
        .unwrap();
    match resumed.outcome {
        RunOutcome::FinalOutput(t) => assert_eq!(t, "all decided"),
        other => panic!("expected final output, got {other:?}"),
    }
}

/// A tool that requires approval — exercises the Interruption/resume path.
pub(crate) struct GuardedTool {
    name: String,
}
impl GuardedTool {
    pub(crate) fn new() -> Self {
        Self {
            name: "guarded".into(),
        }
    }
}
impl Tool for GuardedTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "a tool that needs human approval"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        input: serde_json::Value,
    ) -> houyicoder_async::PFut<'_, Result<serde_json::Value, ToolError>> {
        Box::pin(async move { Ok(serde_json::json!({"ran": input})) })
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

/// A provider whose stream emits the given LlmEvents then never yields again (esc-interrupt path).
pub(crate) struct HangingProvider {
    events: Vec<houyicoder_protocol::llm::LlmEvent>,
}

impl HangingProvider {
    pub(crate) fn new(events: Vec<houyicoder_protocol::llm::LlmEvent>) -> Self {
        Self { events }
    }
}

impl ModelProvider for HangingProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async {
            Ok(CompletionResponse {
                output: vec![],
                usage: Usage::default(),
                model: "test".into(),
            })
        })
    }
    fn capabilities(&self) -> houyicoder_protocol::llm::ModelCapabilities {
        houyicoder_protocol::llm::ModelCapabilities::default()
    }
    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<houyicoder_protocol::llm::LlmEvent, ProviderError>>
    {
        let prefix = futures::stream::iter(self.events.clone().into_iter().map(Ok));
        let tail = futures::stream::pending();
        Box::pin(prefix.chain(tail))
    }
}

#[tokio::test]
async fn test_abort_flushes_partial() {
    // One text delta then a hang: abort must surface RunOutcome::Interrupted
    // and the partial delta must land as an authoritative AssistantMessage.
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(HangingProvider::new(vec![LlmEvent::TextDelta {
        id: "t1".into(),
        text: "partial answer".into(),
    }]));
    let runner = Arc::new(runner_with(p, ToolRegistry::new()));
    let session = SessionId::new();
    let r = runner.clone();
    let task = tokio::spawn(async move { r.run(session, "hi".into()).await });
    // Let the run enter the streaming select! (yields until the pending tail).
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    runner.abort();
    let result = task.await.expect("run task").expect("run ok");
    match result.outcome {
        RunOutcome::Interrupted(reason) => assert_eq!(reason, "interrupted by user"),
        other => panic!("expected Interrupted, got {:?}", other),
    }
    let events = runner.store().replay(session).await.expect("replay");
    let msg = events
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::AssistantMessage { .. }))
        .count();
    assert_eq!(
        msg, 1,
        "partial text flushed as authoritative AssistantMessage"
    );
    // No orphan ToolResults when there were no tool calls.
    let results = events
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::ToolResult { .. }))
        .count();
    assert_eq!(results, 0);
}

#[tokio::test]
async fn test_abort_reconciles_orphan_results() {
    // A tool-call delta (approval-requiring tool) then a hang: abort must flush
    // the ToolCall to the log and reconcile an interrupted-by-user ToolResult
    // for it so the session stays lossless and resumable.
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(HangingProvider::new(vec![LlmEvent::ToolCall {
        id: "c1".into(),
        name: "guarded".into(),
        input: serde_json::json!({"x": 1}),
    }]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let runner = Arc::new(runner_with(p, tools));
    let session = SessionId::new();
    let r = runner.clone();
    let task = tokio::spawn(async move { r.run(session, "hi".into()).await });
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    runner.abort();
    let result = task.await.expect("run task").expect("run ok");
    assert!(matches!(result.outcome, RunOutcome::Interrupted(_)));
    let events = runner.store().replay(session).await.expect("replay");
    let calls = events
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::ToolCall { .. }))
        .count();
    assert_eq!(calls, 1, "tool call flushed to log on abort");
    let orphan_results: Vec<&TurnEventKind> = events
        .iter()
        .map(|e| &e.kind)
        .filter(|k| matches!(k, TurnEventKind::ToolResult { .. }))
        .collect();
    assert_eq!(
        orphan_results.len(),
        1,
        "one reconciled ToolResult for the orphan call"
    );
    if let TurnEventKind::ToolResult {
        call_id, output, ..
    } = orphan_results[0]
    {
        assert_eq!(call_id, "c1");
        assert_eq!(output, &serde_json::json!({"error": "interrupted by user"}));
    } else {
        unreachable!();
    }
}

#[test]
fn test_snapshot_override_drives_prune() {
    // with_snapshot_retention(ttl, 0) sets size cap to 0, which prunes every
    // snapshot. If prune_snapshots still used the default 1GB cap, nothing
    // would be removed. Guards the retention-override wiring from regressing
    // to hardcodes.
    use std::sync::{Arc, Mutex};
    let tmp = std::env::temp_dir().join(format!("t7-ret-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("f.txt"), "x").unwrap();
    let store = Arc::new(crate::snapshot::SnapshotStore::new(&tmp).unwrap());
    store.snapshot(&[]).unwrap();
    store.snapshot(&[]).unwrap();
    let snap_dir = tmp.join(".houyicoder").join("snapshots");
    let before = std::fs::read_dir(&snap_dir).map(|d| d.count()).unwrap_or(0);
    let stack = Arc::new(Mutex::new(crate::snapshot::UndoStack::new()));
    let p = Arc::new(FakeProvider::text("done"));
    let mut r =
        runner_with(p, ToolRegistry::new()).with_snapshot_retention(DEFAULT_SNAPSHOT_TTL_SECS, 0);
    r.set_undo(stack, store);
    r.prune_snapshots();
    let after = std::fs::read_dir(&snap_dir).map(|d| d.count()).unwrap_or(0);
    assert!(
        after < before,
        "size cap 0 override should prune snapshots (before={before}, after={after})"
    );
    std::fs::remove_dir_all(&tmp).ok();
}
