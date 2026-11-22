//! What a delegation reports when its own ceiling ends the run.

use std::future::pending;

use houyicoder_api::tool::Tool;
use houyicoder_api::tool::progress::ToolProgressUpdate;
use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use houyicoder_protocol::extension::ToolError;
use serde_json::{Value, json};
use tokio::time::sleep;

use super::*;

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
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.timeout_secs = Some(1);

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
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.timeout_secs = Some(1);

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
