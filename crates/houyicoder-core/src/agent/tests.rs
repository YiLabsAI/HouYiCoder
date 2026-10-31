use crate::agent::runner_config::DEFAULT_SNAPSHOT_TTL_SECS;
use crate::agent::*;
use crate::provider::test_support::FakeProvider;
use crate::snapshot::SnapshotStore;
use crate::snapshot::UndoStack;
use futures::Stream;
use futures::StreamExt;
use futures::stream::iter;
use futures::stream::poll_fn;
use houyicoder_api::agent_event::{
    AgentEventHandlers, ResponseStreamEvent, RunCompletionStatus, RunLifecycleEvent,
};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_context::SessionEvent;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::InputItem;
use houyicoder_protocol::llm::LlmEvent;
use houyicoder_protocol::llm::ModelCapabilities;
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::llm::{CompletionRequest, CompletionResponse, OutputItem, ProviderError};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::remove_dir_all;
use std::fs::write;
use std::process::id;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::task::Poll;
pub(crate) mod abort_tool;
mod ask_question;
mod budget_pressure_gate;
mod denied_agents;
mod hook_gate;
mod missing_args;
pub(crate) mod run_completion;
mod stream_events;
mod tool_duration;
mod tool_result_baseline;
mod turn_abort;
mod turn_usage_emit;

#[cfg(test)]
mod mid_turn;

#[cfg(test)]
mod queue_lifecycle;

#[cfg(test)]
mod isolate;

#[cfg(test)]
mod compress;

#[cfg(test)]
mod abort_boundary;

#[cfg(test)]
mod overflow;

#[cfg(test)]
mod cache_boundary;

// length is pub(crate): turn_usage_emit_tests (a sibling in agent) reuses
// ScriptRawProvider from it.
#[cfg(test)]
pub(crate) mod length;

#[cfg(test)]
mod memory_gates;

pub(crate) fn runner_with(provider: Arc<dyn ModelProvider>, tools: ToolRegistry) -> Runner {
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

/// A run whose cumulative token spend crosses the configured budget stops
/// before the next model call and surfaces BudgetExceeded with the spent total
/// and the limit, so a runaway turn does not bill past the ceiling.
#[tokio::test]
async fn test_run_budget_stops_turn() {
    // One response that calls a tool and reports 1000 tokens. The loop runs
    // turn 1 (cumulative 1000), the tool answers, RunAgain; turn 2 sees the
    // budget spent and stops before the second model call.
    let resp = CompletionResponse {
        output: vec![
            OutputItem::Text {
                text: "echoing".into(),
            },
            OutputItem::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
        ],
        usage: Usage {
            total_tokens: 1000,
            ..Usage::default()
        },
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(StubTool::new("echo")));
    let runner = runner_with(p, tools).with_run_budget(500);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    match result.outcome {
        RunOutcome::BudgetExceeded { spent, limit } => {
            assert_eq!(spent, 1000, "the spent total is the cumulative usage");
            assert_eq!(limit, 500, "the limit is the configured budget");
        }
        other => panic!("expected BudgetExceeded, got {other:?}"),
    }
    assert_eq!(
        result.turns, 1,
        "the budget stops before the second model call"
    );
}

/// The run budget is per run, not session-wide: a second run in the same
/// session starts with a fresh spend, so the first run's bill does not
/// pre-block the next before its first model call.
#[tokio::test]
async fn test_budget_scoped_to_run() {
    let resp = CompletionResponse {
        output: vec![
            OutputItem::Text {
                text: "echoing".into(),
            },
            OutputItem::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
        ],
        usage: Usage {
            total_tokens: 1000,
            ..Usage::default()
        },
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(StubTool::new("echo")));
    let runner = runner_with(p, tools).with_run_budget(500);
    let session = SessionId::new();
    let first = runner.run(session, "first".into()).await.unwrap();
    assert!(
        matches!(first.outcome, RunOutcome::BudgetExceeded { .. }),
        "the first run hits the budget: {:?}",
        first.outcome
    );
    let second = runner.run(session, "second".into()).await.unwrap();
    assert!(
        matches!(second.outcome, RunOutcome::BudgetExceeded { .. }),
        "the second run also hits its own budget: {:?}",
        second.outcome
    );
    assert_eq!(
        second.turns, 1,
        "the second run is not pre-blocked: it runs a turn before the budget fires"
    );
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
async fn test_resume_preserves_turn_budget() {
    // An approval-requiring tool forces an Interruption every turn. With
    // max_turns=1, a user turn spans run() + its resumes, so resume() must
    // carry the per-turn budget (1) into the next iteration to hit the cap;
    // a fresh budget on resume would let the turn escape its bound.
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
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
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
async fn test_max_turns_resets() {
    // A user turn that crossed the cap (run + approval resume) must not
    // permanently cap the session: the next run() starts a fresh per-turn
    // budget, so a long-lived session stays usable after any one turn burns
    // its max. The approval tool forces an Interruption each turn; the
    // provider repeats the tool call so every turn asks again.
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
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        p,
        tools,
        RunnerConfig {
            max_turns: 1,
            ..runner_with_cfg0()
        },
    );
    let session = SessionId::new();
    let first = runner.run(session, "first".into()).await.unwrap();
    let approvals = match first.outcome {
        RunOutcome::Interruption(a) => a,
        other => panic!("expected interruption, got {other:?}"),
    };
    let decisions: Vec<ApprovalDecision> = approvals
        .iter()
        .map(|a| ApprovalDecision::approve(&a.call_id))
        .collect();
    let capped = runner.resume(session, &decisions).await.unwrap();
    assert!(
        matches!(capped.outcome, RunOutcome::MaxTurnsReached { .. }),
        "resume after the cap crosses max_turns, got {:?}",
        capped.outcome
    );
    // A new user turn re-enters run(): the budget resets, so the first turn
    // is allowed again instead of being instantly capped.
    let retry = runner.run(session, "second".into()).await.unwrap();
    assert!(
        matches!(retry.outcome, RunOutcome::Interruption(_)),
        "a fresh run after max_turns gets a new budget, got {:?}",
        retry.outcome
    );
}

#[tokio::test]
async fn test_max_turns_scope() {
    // The max_turns message must report the CURRENT user turn's count, not
    // the session's accumulated total. A prior completed turn leaves the
    // session at one TurnStarted; the next turn capped at max_turns=1 must
    // report 1, not drift to 2.
    let text = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let tool = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "guarded".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![text, tool]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        p,
        tools,
        RunnerConfig {
            max_turns: 1,
            ..runner_with_cfg0()
        },
    );
    let session = SessionId::new();
    let first = runner.run(session, "first".into()).await.unwrap();
    assert!(matches!(first.outcome, RunOutcome::FinalOutput(_)));
    let second = runner.run(session, "second".into()).await.unwrap();
    let approvals = match second.outcome {
        RunOutcome::Interruption(a) => a,
        other => panic!("expected interruption, got {other:?}"),
    };
    let decisions: Vec<ApprovalDecision> = approvals
        .iter()
        .map(|a| ApprovalDecision::approve(&a.call_id))
        .collect();
    let capped = runner.resume(session, &decisions).await.unwrap();
    assert!(
        matches!(capped.outcome, RunOutcome::MaxTurnsReached { turns } if turns == 1),
        "max_turns must report the current turn's count, got {:?}",
        capped.outcome
    );
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

pub(crate) fn runner_with_cfg0() -> RunnerConfig {
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

/// The paused-run shape on a caller's store, so a test can choose what the log
/// does when the released call is written.
pub(crate) fn guarded_runner(store: Arc<SessionStore>) -> Runner {
    let resp = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "guarded".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    Runner::new(
        store,
        Arc::new(FakeProvider::new(vec![resp])),
        tools,
        RunnerConfig {
            max_turns: 1,
            ..runner_with_cfg0()
        },
    )
}

/// The approvals a paused run raised, so a resume can answer them.
pub(crate) fn approvals_of(outcome: RunOutcome) -> Vec<ApprovalRequest> {
    match outcome {
        RunOutcome::Interruption(a) => a,
        other => panic!("expected the guarded call to pause for approval, got {other:?}"),
    }
}

#[tokio::test]
async fn test_stream_persists_deltas() {
    // Response handlers receive each streamed delta. The session log carries
    // only the authoritative assistant message, and the assembly produces one
    // assistant input item without duplicating transport-only deltas.
    let p = Arc::new(FakeProvider::text("hello world"));
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let collected: Arc<Mutex<Vec<ResponseStreamEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let mut events = AgentEventHandlers::default();
    let captured = collected.clone();
    events.set_response_stream(Arc::new(move |event| {
        captured.lock().expect("events").push(event);
    }));
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
    runner.set_event_handlers(events);
    let result = runner.run(session, "hi".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(t) if t == "hello world"));

    let streamed = collected.lock().expect("events").clone();
    let deltas: Vec<String> = streamed
        .into_iter()
        .map(|event| match event {
            ResponseStreamEvent::AssistantTextDelta { text }
            | ResponseStreamEvent::ReasoningDelta { text } => text,
            ResponseStreamEvent::ToolInputDelta { text } => text,
        })
        .collect();
    assert_eq!(deltas.concat(), "hello world");
    assert_eq!(deltas.len(), 3);

    // The durable log carries only the authoritative AssistantMessage.
    // Response events carry deltas without fragmenting model-input history.
    let events = runner.store().replay(session).await.expect("replay");
    let delta_n = events
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::AssistantTextDelta { .. }))
        .count();
    assert_eq!(
        delta_n, 0,
        "deltas are transport-only, not in the durable log"
    );
    let msg_n = events
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::AssistantMessage { .. }))
        .count();
    assert_eq!(msg_n, 1, "one authoritative AssistantMessage");
    let items = assemble_model_input(&events, None);
    let assistant = items
        .iter()
        .filter(|i| matches!(i, InputItem::Assistant { .. }))
        .count();
    assert_eq!(assistant, 1, "the assembly folds deltas into one Assistant");
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
                &e.event,
                SessionEvent::ToolResult { call_id, .. } if call_id == cid
            )
        })
    };
    assert!(has_result("c1"), "decided call got a result");
    assert!(!has_result("c2"), "undecided call left pending");
}

#[tokio::test]
async fn test_resume_emits_final_completion() {
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
    let captured = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::clone(&captured);
    let mut handlers = AgentEventHandlers::default();
    handlers.set_run_lifecycle(Arc::new(move |event| {
        events.lock().expect("lifecycle events").push(event);
    }));
    let mut runner = runner_with(p, tools);
    runner.set_event_handlers(handlers);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    assert!(
        captured.lock().expect("lifecycle events").is_empty(),
        "approval pause must not complete the run"
    );
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
    assert_eq!(
        *captured.lock().expect("lifecycle events"),
        vec![RunLifecycleEvent::Completed {
            status: RunCompletionStatus::Completed,
            summary: "after approvals".into(),
        }]
    );
    // Verify the reject branch wrote the rejection-note result for c2.
    let events = runner.store().replay(session).await.unwrap();
    let c2_outcome = events.iter().find_map(|e| match &e.event {
        SessionEvent::ToolResult {
            call_id, output, ..
        } if call_id == "c2" => Some(output.clone()),
        _ => None,
    });
    assert_eq!(
        c2_outcome,
        Some(serde_json::json!({
            "error": "rejected by user",
            "not_run": true
        }))
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
        .filter(|e| matches!(e.event, SessionEvent::ToolResult { .. }))
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

/// The tail of a stream that never yields another item, plus one cancel to
/// perform the first time the run reaches it. Cancelling there is what makes
/// an interrupt test deterministic: the cancel lands while the run waits on
/// the model, instead of depending on a count of scheduler yields that a slow
/// build can outlast.
pub(crate) struct TailCancel {
    at_tail: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl TailCancel {
    pub(crate) fn new() -> Self {
        Self {
            at_tail: OnceLock::new(),
        }
    }

    /// Cancel the run at the tail. The first installation wins; Weak keeps
    /// the provider from holding the runner alive.
    pub(crate) fn abort_run_at_tail(&self, runner: &Arc<Runner>) {
        let runner = Arc::downgrade(runner);
        drop(self.at_tail.set(Box::new(move || {
            if let Some(runner) = runner.upgrade() {
                runner.abort();
            }
        })));
    }

    /// Cancel the active turn at the tail, leaving the run alive.
    pub(crate) fn cancel_turn_at_tail(&self, runner: &Arc<Runner>) {
        let runner = Arc::downgrade(runner);
        drop(self.at_tail.set(Box::new(move || {
            if let Some(runner) = runner.upgrade() {
                runner.cancel_turn();
            }
        })));
    }

    /// The tail itself: perform the installed cancel on the first poll, then
    /// wait for an item that never comes.
    pub(crate) fn tail(&self) -> impl Stream<Item = Result<LlmEvent, ProviderError>> + '_ {
        let mut fired = false;
        poll_fn(move |_cx| {
            if !fired {
                fired = true;
                if let Some(cancel) = self.at_tail.get() {
                    cancel();
                }
            }
            Poll::<Option<Result<LlmEvent, ProviderError>>>::Pending
        })
    }
}

/// A provider whose stream emits the given LlmEvents then never yields again
/// (esc-interrupt path). A test can cancel the run the moment the consumer
/// reaches the tail, so the interrupt lands where the run is waiting on the
/// model rather than after a guess at how many yields it needs to get there.
pub(crate) struct HangingProvider {
    events: Vec<LlmEvent>,
    tail: TailCancel,
}

impl HangingProvider {
    pub(crate) fn new(events: Vec<LlmEvent>) -> Self {
        Self {
            events,
            tail: TailCancel::new(),
        }
    }

    /// Abort the run once it has read every event and waits on this stream.
    pub(crate) fn abort_at_tail(&self, runner: &Arc<Runner>) {
        self.tail.abort_run_at_tail(runner);
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
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
        let prefix = iter(self.events.clone().into_iter().map(Ok));
        Box::pin(prefix.chain(self.tail.tail()))
    }
}

#[tokio::test]
async fn test_abort_flushes_partial() {
    // One text delta then a hang: abort must surface RunOutcome::Interrupted
    // and the partial delta must land as an authoritative AssistantMessage.
    let p = Arc::new(HangingProvider::new(vec![LlmEvent::TextDelta {
        id: "t1".into(),
        text: "partial answer".into(),
    }]));
    let runner = Arc::new(runner_with(p.clone(), ToolRegistry::new()));
    p.abort_at_tail(&runner);
    let session = SessionId::new();
    let r = runner.clone();
    let task = tokio::spawn(async move { r.run(session, "hi".into()).await });
    let result = task.await.expect("run task").expect("run ok");
    match result.outcome {
        RunOutcome::Interrupted(reason) => assert_eq!(reason, "interrupted by user"),
        other => panic!("expected Interrupted, got {:?}", other),
    }
    let events = runner.store().replay(session).await.expect("replay");
    let msg = events
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::AssistantMessage { .. }))
        .count();
    assert_eq!(
        msg, 1,
        "partial text flushed as authoritative AssistantMessage"
    );
    // No orphan ToolResults when there were no tool calls.
    let results = events
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::ToolResult { .. }))
        .count();
    assert_eq!(results, 0);
}

#[tokio::test]
async fn test_abort_reconciles_orphan_results() {
    // A tool-call delta (approval-requiring tool) then a hang: abort must flush
    // the ToolCall to the log and reconcile an interrupted-by-user ToolResult
    // for it so the session stays lossless and resumable.
    let p = Arc::new(HangingProvider::new(vec![LlmEvent::ToolCall {
        id: "c1".into(),
        name: "guarded".into(),
        input: serde_json::json!({"x": 1}),
    }]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let runner = Arc::new(runner_with(p.clone(), tools));
    p.abort_at_tail(&runner);
    let session = SessionId::new();
    let r = runner.clone();
    let task = tokio::spawn(async move { r.run(session, "hi".into()).await });
    let result = task.await.expect("run task").expect("run ok");
    assert!(matches!(result.outcome, RunOutcome::Interrupted(_)));
    let events = runner.store().replay(session).await.expect("replay");
    let calls = events
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::ToolCall { .. }))
        .count();
    assert_eq!(calls, 1, "tool call flushed to log on abort");
    let orphan_results: Vec<&SessionEvent> = events
        .iter()
        .map(|e| &e.event)
        .filter(|k| matches!(k, SessionEvent::ToolResult { .. }))
        .collect();
    assert_eq!(
        orphan_results.len(),
        1,
        "one reconciled ToolResult for the orphan call"
    );
    if let SessionEvent::ToolResult {
        call_id, output, ..
    } = orphan_results[0]
    {
        assert_eq!(call_id, "c1");
        assert_eq!(
            output,
            &serde_json::json!({"error": "interrupted by user", "not_run": true})
        );
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
    let tmp = temp_dir().join(format!("t7-ret-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).unwrap();
    write(tmp.join("f.txt"), "x").unwrap();
    let store = Arc::new(SnapshotStore::new(&tmp).unwrap());
    store.snapshot(&[]).unwrap();
    store.snapshot(&[]).unwrap();
    let snap_dir = tmp.join(".houyicoder").join("snapshots");
    let before = read_dir(&snap_dir).map(|d| d.count()).unwrap_or(0);
    let stack = Arc::new(Mutex::new(UndoStack::new()));
    let p = Arc::new(FakeProvider::text("done"));
    let mut r =
        runner_with(p, ToolRegistry::new()).with_snapshot_retention(DEFAULT_SNAPSHOT_TTL_SECS, 0);
    r.set_undo(stack, store);
    r.prune_snapshots();
    let after = read_dir(&snap_dir).map(|d| d.count()).unwrap_or(0);
    assert!(
        after < before,
        "size cap 0 override should prune snapshots (before={before}, after={after})"
    );
    remove_dir_all(&tmp).ok();
}
