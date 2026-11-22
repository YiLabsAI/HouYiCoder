//! What a delegation reports when its own ceiling ends the run.

use std::future::pending;

use futures::stream::once;
use houyicoder_api::provider::stream_from_response;
use houyicoder_api::tool::Tool;
use houyicoder_api::tool::progress::ToolProgressUpdate;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{SessionId, SessionLogEntry};
use houyicoder_core::agent::multi_agent::summarize_watchdog_returns;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{
    CompletionRequest, LlmEvent, ModelCapabilities, ProviderError, UsageProvenance,
};
use serde_json::{Value, json};
use tokio::time::sleep;

use super::*;

/// A short timeout_secs hint passed directly to the runtime is floored at the
/// 3600s safety ceiling rather than cutting off a healthy delegation early.
#[tokio::test]
async fn test_runtime_floors_short_timeout() {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new_with_delay(
        vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: "finished past 1s".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }],
        1_100,
    ));
    let (runtime, _store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.timeout_secs = Some(1);

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");
    assert_eq!(
        outcome.status.as_deref(),
        Some("completed"),
        "short runtime timeout_secs must floor at 3600s instead of cutting at 1s"
    );
    assert_eq!(outcome.summary.as_deref(), Some("finished past 1s"));
    assert_eq!(
        spawn_exec::effective_wall_clock(Some(30), spawn_exec::DEFAULT_MAX_WALL_CLOCK),
        Duration::from_secs(3600)
    );
    assert_eq!(
        spawn_exec::effective_wall_clock(Some(7200), spawn_exec::DEFAULT_MAX_WALL_CLOCK),
        Duration::from_secs(7200)
    );
}

/// A child that outlives its ceiling is stopped and reported as such.
///
/// The run used to end by dropping the task, which dismantled the child
/// without running the tail that publishes its terminal, closes its inbox, and
/// releases the fence slot, and the parent got an error naming nothing the
/// child had done. The ceiling now stops the child the way a shutdown does and
/// reports the ending, so the parent reads a status it can act on.
#[tokio::test]
async fn test_ceiling_stops_child() {
    // A provider that pauses before every response, so the child cannot reach
    // a terminal of its own inside the ceiling below.
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new_with_delay(
        vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: "unreachable".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }],
        5_000,
    ));
    let (runtime, _store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let runtime = runtime.with_test_wall_clock(Duration::from_secs(1));
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "task", "task");

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");

    assert_eq!(
        outcome.status.as_deref(),
        Some("timed_out"),
        "the ceiling is named as the thing that ended the run"
    );
    let summary = outcome.summary.unwrap_or_default();
    assert!(
        summary.contains("timed out after 1s"),
        "the report says how long it was given: {summary}"
    );
    assert!(
        summary.contains("resumable via"),
        "and says the work can be continued: {summary}"
    );
    assert!(
        summary.contains(&outcome.child_session_id),
        "naming the session that holds the work: {summary}"
    );
}

/// A delegation cut by its ceiling reports what the child had produced, and
/// the same ending reaches the bus: the parent reads one of the two and the
/// user reads the other, so an interrupt on one side would make the two
/// accounts of one run disagree.
#[tokio::test]
async fn test_timeout_keeps_partial() {
    // The first model call answers with text and a call to a tool that is not
    // registered, so the child has output on record and asks for one more
    // call; that call never returns.
    let (provider, _stalled) = StallAfterFirstProvider::new(vec![vec![
        OutputItem::Text {
            text: "half an answer".into(),
        },
        OutputItem::ToolCall {
            id: "tc1".into(),
            name: "ghost".into(),
            input: serde_json::json!({}),
        },
    ]]);
    let bus = Arc::new(AgentBus::new());
    let mut completed = bus.subscribe(global_completed_topic());
    let (runtime, _store, parent_sid) =
        runtime_with(provider, Some(Arc::clone(&bus)), ToolRegistry::new());
    let runtime = runtime.with_test_wall_clock(Duration::from_secs(1));
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "task", "task");

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");

    let summary = outcome.summary.unwrap_or_default();
    assert!(
        summary.contains("half an answer"),
        "the report keeps what the child had produced: {summary}"
    );
    assert!(
        summary.contains(&outcome.child_session_id),
        "and names the session to continue: {summary}"
    );
    let message = completed.try_recv().expect("the ending reached the bus");
    assert!(
        matches!(
            message,
            BusMessage::Completed {
                status: CompletionStatus::TimedOut,
                ..
            }
        ),
        "the bus reports the same ending as the caller: {message:?}"
    );
}

/// A child that finishes within its ceiling reports its own ending, so the
/// timeout path does not relabel work that completed.
#[tokio::test]
async fn test_ceiling_leaves_finished_alone() {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("done"));
    let (runtime, _store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.timeout_secs = Some(60);

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");

    assert_eq!(
        outcome.status.as_deref(),
        Some("completed"),
        "a run inside its ceiling keeps its own ending"
    );
    assert_eq!(outcome.summary.as_deref(), Some("done"));
}

struct HungTool;

impl Tool for HungTool {
    fn name(&self) -> &str {
        "hung_probe"
    }
    fn description(&self) -> &str {
        "stalls until cancelled"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute(&self, ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            match ctx.cancel {
                Some(token) => token.cancelled().await,
                None => pending::<()>().await,
            }
            Ok(json!({"interrupted": true}))
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn requires_approval(&self) -> bool {
        false
    }
}

/// When a child subagent stalls inside a quiet tool call before emitting any
/// assistant text, the watchdog triggers a soft wrap-up turn that cancels the
/// stuck tool, refuses any further tool attempts during wrap-up, and lets the
/// responsive model synthesize its partial report.
#[tokio::test]
async fn test_stalled_tool_wraps_up() {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "tc1".into(),
                name: "hung_probe".into(),
                input: json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "tc2".into(),
                name: "hung_probe".into(),
                input: json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "Findings: verified module A; Unverified: hung_probe timed out.".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HungTool));
    let (runtime, store, parent_sid) = runtime_with(provider, None, tools);
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "audit", "audit");
    args.timeout_secs = Some(1);

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");
    assert_eq!(outcome.status.as_deref(), Some("timed_out"));
    let summary = outcome.summary.unwrap_or_default();
    assert!(
        summary.contains("Findings: verified module A"),
        "expected synthesized wrap-up report in timed-out summary, got: {summary}"
    );
    let child_sid =
        SessionId::from_display_string(&outcome.child_session_id).expect("valid child sid");
    let child_events = store.replay(child_sid).await.expect("child replay");
    assert!(
        child_events.iter().any(|ev| matches!(
            &ev.event,
            SessionEvent::MetaUser { text } if text.contains("time budget has ended")
        )),
        "wrap-up directive must be logged as MetaUser"
    );
    assert!(
        !child_events
            .iter()
            .any(|ev| matches!(&ev.event, SessionEvent::MidTurnInput { .. })),
        "wrap-up directive must never be logged as MidTurnInput"
    );
}

struct SilentProgressTool;

impl Tool for SilentProgressTool {
    fn name(&self) -> &str {
        "silent_progress"
    }
    fn description(&self) -> &str {
        "emits empty-tail progress ticks across the quiet window"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute(&self, ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            for elapsed in 0..8 {
                if let Some(progress) = &ctx.progress {
                    progress.update(ToolProgressUpdate {
                        elapsed_secs: elapsed,
                        timeout_secs: Some(600),
                        tail: Vec::new(),
                    });
                }
                sleep(Duration::from_millis(25)).await;
            }
            Ok(json!({"ok": true}))
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn requires_approval(&self) -> bool {
        false
    }
}

/// A redirected command that emits progress ticks with no new pipe output
/// survives past the tool quiet window rather than being cut prematurely.
#[tokio::test]
async fn test_silent_progress_tool_survives() {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "tc1".into(),
                name: "silent_progress".into(),
                input: json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "completed after redirected run".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SilentProgressTool));
    let (runtime, _store, parent_sid) = runtime_with(provider, None, tools);
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "verify", "verify");
    args.timeout_secs = Some(2);

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");
    assert_eq!(outcome.status.as_deref(), Some("completed"));
    assert_eq!(
        outcome.summary.as_deref(),
        Some("completed after redirected run")
    );
}

fn runtime_sharing_store(
    store: Arc<SessionStore>,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
) -> MultiAgentRuntime {
    MultiAgentRuntime::new(MultiAgentDeps {
        registry: Arc::new(BuiltInRegistry::from_agents(built_in_all())),
        store: store as Arc<dyn SessionLog>,
        provider,
        tools,
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(PathBuf::from("/tmp")),
        bus: None,
        descriptor_store: Some(Arc::new(InMemoryDescriptorStore::new())),
        skill_registry: None,
        conditional: None,
        hooks: None,
    })
}

async fn run_for_metrics(
    store: Arc<SessionStore>,
    parent_sid: SessionId,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    test_floor: Option<Duration>,
) {
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "metric", "metric");
    let mut runtime = runtime_sharing_store(store, provider, tools);
    if let Some(floor) = test_floor {
        runtime = runtime.with_test_wall_clock(floor);
    }
    drop(runtime.spawn(&ctx, args).await.expect("spawn completes"));
}

/// Parent return boundaries record watchdog axis, wrap-up rescue status, and
/// cost-model effective tokens across natural completions, soft-wrap rescues,
/// and hard cancellations.
#[tokio::test]
async fn test_subagent_return_records_metrics() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let natural_provider: Arc<dyn ModelProvider> =
        Arc::new(FakeProvider::new(vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: "all clear".into(),
            }],
            usage: Usage {
                input_tokens: 1000,
                output_tokens: 100,
                total_tokens: 1100,
                non_cached_input_tokens: 200,
                cache_read_input_tokens: 800,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                provenance: UsageProvenance::ALL_REPORTED,
            },
            model: "test".into(),
        }]));
    run_for_metrics(
        Arc::clone(&store),
        parent_sid,
        natural_provider,
        ToolRegistry::new(),
        None,
    )
    .await;

    let wrap_provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "tc1".into(),
                name: "hung_probe".into(),
                input: json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "Findings: rescued partial report".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HungTool));
    run_for_metrics(Arc::clone(&store), parent_sid, wrap_provider, tools, None).await;

    let (stall_provider, _stall) = StallAfterFirstProvider::new(vec![vec![OutputItem::ToolCall {
        id: "tc1".into(),
        name: "ghost".into(),
        input: json!({}),
    }]]);
    run_for_metrics(
        Arc::clone(&store),
        parent_sid,
        stall_provider,
        ToolRegistry::new(),
        Some(Duration::from_secs(1)),
    )
    .await;

    let parent_events = store.replay(parent_sid).await.expect("parent replay");
    assert_recorded_metrics(&parent_events);
}

fn assert_recorded_metrics(parent_events: &[SessionLogEntry]) {
    let returns: Vec<&SessionEvent> = parent_events
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::SubagentReturn { .. }))
        .map(|e| &e.event)
        .collect();
    assert_eq!(returns.len(), 3);
    assert!(matches!(
        returns[0],
        SessionEvent::SubagentReturn {
            cut_axis: None,
            soft_wrapped: Some(false),
            wrap_rescued: None,
            effective_tokens: Some(780),
            ..
        }
    ));
    assert!(matches!(
        returns[1],
        SessionEvent::SubagentReturn {
            cut_axis: Some(axis),
            soft_wrapped: Some(true),
            wrap_rescued: Some(true),
            ..
        } if axis == "tool_quiet"
    ));
    assert!(matches!(
        returns[2],
        SessionEvent::SubagentReturn {
            cut_axis: Some(axis),
            soft_wrapped: Some(true),
            wrap_rescued: Some(false),
            ..
        } if axis == "wall_clock"
    ));

    let summary = summarize_watchdog_returns(parent_events);
    assert_eq!(summary.completed_naturally, 1);
    assert_eq!(summary.soft_wrap_rescued, 1);
    assert_eq!(summary.hard_cut, 1);
    assert_eq!(summary.tool_quiet_cuts, 1);
    assert_eq!(summary.wall_clock_cuts, 1);
}

/// Earlier assistant text or a provider error message after wrap-up must not
/// inflate wrap_rescued: only new non-empty assistant text emitted after the
/// wrap-up MetaUser directive counts as a rescue.
#[tokio::test]
async fn test_rescue_requires_new_text() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();

    // Child 1 emits text on turn 1 before stalling on turn 2; wrap-up stalls
    // too and gets hard-cut. The stale turn-1 text must not count as rescued.
    let (stale_provider, _stall) = StallAfterFirstProvider::new(vec![vec![
        OutputItem::Text {
            text: "half an answer".into(),
        },
        OutputItem::ToolCall {
            id: "tc1".into(),
            name: "ghost".into(),
            input: json!({}),
        },
    ]]);
    run_for_metrics(
        Arc::clone(&store),
        parent_sid,
        stale_provider,
        ToolRegistry::new(),
        Some(Duration::from_secs(1)),
    )
    .await;

    // Child 2 enters tool-quiet wrap-up and then fails with a provider error;
    // the "run failed: ..." error summary must not count as rescued.
    let fail_after_wrap: Arc<dyn ModelProvider> =
        Arc::new(FailAfterFirstProvider::new(vec![CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "tc1".into(),
                name: "hung_probe".into(),
                input: json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HungTool));
    run_for_metrics(Arc::clone(&store), parent_sid, fail_after_wrap, tools, None).await;

    let events = store.replay(parent_sid).await.expect("parent replay");
    let returns: Vec<&SessionEvent> = events
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::SubagentReturn { .. }))
        .map(|e| &e.event)
        .collect();
    assert_eq!(returns.len(), 2);
    assert!(
        matches!(
            returns[0],
            SessionEvent::SubagentReturn {
                soft_wrapped: Some(true),
                wrap_rescued: Some(false),
                summary,
                ..
            } if summary.contains("half an answer")
        ),
        "stale earlier text must not mark wrap_rescued=true: {:?}",
        returns[0]
    );
    assert!(
        matches!(
            returns[1],
            SessionEvent::SubagentReturn {
                soft_wrapped: Some(true),
                wrap_rescued: Some(false),
                ..
            }
        ),
        "provider failure during wrap-up must not mark wrap_rescued=true: {:?}",
        returns[1]
    );
}

struct FailAfterFirstProvider {
    first: Mutex<Option<CompletionResponse>>,
}

impl FailAfterFirstProvider {
    fn new(mut items: Vec<CompletionResponse>) -> Self {
        Self {
            first: Mutex::new(items.drain(..).next()),
        }
    }
}

impl ModelProvider for FailAfterFirstProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let next = self.first.lock().expect("first lock").take();
        Box::pin(async move {
            match next {
                Some(resp) => Ok(resp),
                None => Err(ProviderError::Auth),
            }
        })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let next = self.first.lock().expect("first lock").take();
        match next {
            Some(resp) => stream_from_response(resp),
            None => Box::pin(once(async { Err(ProviderError::Auth) })),
        }
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}
