//! Mid-turn interjection tests: the host queueing a user message mid-run
//! (the runner picks it up after the current tool returns). Uses the parent
//! tests module's fixtures via the glob import.
use super::*;

// A tool that enqueues a fixed string onto the runner's mid-turn queue when
// it executes, simulating the host queueing a user message mid-run (the host
// service would call enqueue_input on receiving the inject wire; the test
// calls it directly via a shared Arc<Runner> slot set after construction).
struct QueuingTool {
    slot: std::sync::Arc<std::sync::OnceLock<Arc<Runner>>>,
    msg: String,
}
impl QueuingTool {
    fn new(slot: std::sync::Arc<std::sync::OnceLock<Arc<Runner>>>, msg: &str) -> Self {
        Self {
            slot,
            msg: msg.into(),
        }
    }
}
impl Tool for QueuingTool {
    fn name(&self) -> &str {
        "queue_user"
    }
    fn description(&self) -> &str {
        "test tool: queues a user message"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: serde_json::Value,
    ) -> houyicoder_async::PFut<'_, Result<serde_json::Value, ToolError>> {
        if let Some(r) = self.slot.get() {
            r.enqueue_input(self.msg.clone());
        }
        Box::pin(async move { Ok(serde_json::json!({"queued": true})) })
    }
}

// A provider that records the projected input of each call + returns a
// scripted response sequence. Call 1 emits the queueing tool call so the
// run continues (RunAgain); call 2 returns final text so the loop ends.
struct CaptureProvider {
    calls: std::sync::Mutex<usize>,
    seen: std::sync::Mutex<Vec<String>>,
    tool_name: String,
}
impl CaptureProvider {
    fn new() -> Self {
        Self {
            calls: std::sync::Mutex::new(0),
            seen: std::sync::Mutex::new(Vec::new()),
            tool_name: "queue_user".into(),
        }
    }
    fn with_tool(name: &str) -> Self {
        Self {
            calls: std::sync::Mutex::new(0),
            seen: std::sync::Mutex::new(Vec::new()),
            tool_name: name.into(),
        }
    }
    fn capture_input(&self, req: &CompletionRequest) {
        let input = serde_json::to_string(&req.input).unwrap_or_default();
        self.seen
            .lock()
            .expect("seen")
            .push(format!("{}\n{input}", req.instructions));
    }
    fn next_script(&self) -> CompletionResponse {
        let mut c = self.calls.lock().expect("calls");
        *c += 1;
        let n = *c;
        let tool_name = self.tool_name.clone();
        drop(c);
        scripted_capture(n, &tool_name)
    }
}
impl houyicoder_api::provider::ModelProvider for CaptureProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.capture_input(&req);
        let resp = self.next_script();
        Box::pin(async move { Ok(resp) })
    }
    fn stream(
        &self,
        req: CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<houyicoder_protocol::llm::LlmEvent, ProviderError>>
    {
        self.capture_input(&req);
        let resp = self.next_script();
        houyicoder_api::provider::stream_from_response(resp)
    }
    fn capabilities(&self) -> houyicoder_protocol::llm::ModelCapabilities {
        houyicoder_protocol::llm::ModelCapabilities::default()
    }
}
fn scripted_capture(n: usize, tool_name: &str) -> CompletionResponse {
    if n == 1 {
        CompletionResponse {
            output: vec![
                OutputItem::Text {
                    text: "running the task".into(),
                },
                OutputItem::ToolCall {
                    id: "q1".into(),
                    name: tool_name.into(),
                    input: serde_json::json!({}),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        }
    } else {
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }
    }
}

/// A user message enqueued during turn 1 (the tool's execute) is drained at
/// the turn-2 boundary + appended as a user message, so the model's second
/// call sees the interjection in its projected input. This is the
/// turn-boundary injection — finer than the run-boundary queue.
#[tokio::test]
async fn test_turn_injects_between_turns() {
    let slot = std::sync::Arc::new(std::sync::OnceLock::<Arc<Runner>>::new());
    let provider = Arc::new(CaptureProvider::new());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(QueuingTool::new(
        std::sync::Arc::clone(&slot),
        "wait, also check the logs",
    )));
    let runner = Arc::new(runner_with(
        Arc::clone(&provider) as Arc<dyn houyicoder_api::provider::ModelProvider>,
        tools,
    ));
    slot.set(Arc::clone(&runner)).ok();
    let session = houyicoder_context::SessionId::new();
    let result = runner
        .run(session, "do the task".into())
        .await
        .expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(t) if t == "done"));
    let seen = provider.seen.lock().expect("seen");
    assert_eq!(seen.len(), 2, "two model calls");
    assert!(
        seen[1].contains("wait, also check the logs"),
        "turn-2 input must carry the queued interjection: {}",
        seen[1]
    );
    assert!(
        seen[1].contains("Continue your current task"),
        "turn-2 input must carry the mid-turn framing (continue + address): {}",
        seen[1]
    );
}

/// Enqueue + remove before the drain: a removed id is not injected, so the
/// model's turn-2 input must not carry the removed text. Covers the
/// overlay-delete wire path (remove_input by id, no-op when already gone).
#[tokio::test]
async fn test_removed_input_not_injected() {
    let slot = std::sync::Arc::new(std::sync::OnceLock::<Arc<Runner>>::new());
    let provider = Arc::new(CaptureProvider::new());
    let mut tools = ToolRegistry::new();
    // The tool enqueues the message then immediately removes it before the
    // drive loop can drain at the next turn boundary.
    tools.register(Arc::new(RemoveAfterEnqueueTool::new(
        std::sync::Arc::clone(&slot),
        "wait, also check the logs",
    )));
    let runner = Arc::new(runner_with(
        Arc::clone(&provider) as Arc<dyn houyicoder_api::provider::ModelProvider>,
        tools,
    ));
    slot.set(Arc::clone(&runner)).ok();
    let session = houyicoder_context::SessionId::new();
    let result = runner
        .run(session, "do the task".into())
        .await
        .expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(t) if t == "done"));
    let seen = provider.seen.lock().expect("seen");
    assert_eq!(seen.len(), 2, "two model calls");
    assert!(
        !seen[1].contains("wait, also check the logs"),
        "turn-2 input must not carry the removed interjection: {}",
        seen[1]
    );
}

struct RemoveAfterEnqueueTool {
    slot: std::sync::Arc<std::sync::OnceLock<Arc<Runner>>>,
    msg: String,
}
impl RemoveAfterEnqueueTool {
    fn new(slot: std::sync::Arc<std::sync::OnceLock<Arc<Runner>>>, msg: &str) -> Self {
        Self {
            slot,
            msg: msg.into(),
        }
    }
}
impl Tool for RemoveAfterEnqueueTool {
    fn name(&self) -> &str {
        "enqueue_then_remove"
    }
    fn description(&self) -> &str {
        "test tool: enqueues then removes a user message"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: serde_json::Value,
    ) -> houyicoder_async::PFut<'_, Result<serde_json::Value, ToolError>> {
        if let Some(r) = self.slot.get() {
            r.enqueue_input(self.msg.clone());
            r.remove_input(&self.msg);
        }
        Box::pin(async move { Ok(serde_json::json!({"queued": true})) })
    }
}

/// A hard crash can leave an orphan ToolCall (logged, no ToolResult). On the
/// next run(), the entry reconcile appends an interrupted ToolResult before
/// this turn's user input so build_request_body ships role:"tool" right after
/// the assistant turn that issued the call — else the provider 400s. Assert on
/// the projection (END: model input legal), not on event existence (MEANS):
/// every Assistant with tool_calls must be immediately followed by ToolResult
/// items whose call_id set equals the tool_call id set. Order-sensitive —
/// no-fix red, wrapper-site red, run-entry green.
#[tokio::test]
async fn test_run_repairs_orphan_call() {
    use houyicoder_context::TurnEventKind;
    use houyicoder_protocol::llm::{InputItem, OutputItem};
    let runner = Arc::new(runner_with(
        Arc::new(crate::provider::test_support::FakeProvider::new(vec![
            CompletionResponse {
                output: vec![OutputItem::Text {
                    text: "done".into(),
                }],
                usage: Usage::default(),
                model: "test".into(),
            },
        ])),
        ToolRegistry::new(),
    ));
    let session = houyicoder_context::SessionId::new();
    // Simulate a crash that logged a ToolCall but not its ToolResult.
    runner
        .store()
        .append(new_event(
            session,
            TurnEventKind::AssistantMessage {
                text: String::new(),
                thinking: None,
            },
        ))
        .await
        .expect("append assistant");
    runner
        .store()
        .append(new_event(
            session,
            TurnEventKind::ToolCall {
                call_id: "c1".into(),
                tool: "echo".into(),
                input: serde_json::json!({}),
            },
        ))
        .await
        .expect("append tool call");
    let result = runner.run(session, "follow-up".into()).await.expect("run");
    assert!(
        matches!(result.outcome, RunOutcome::FinalOutput(ref t) if t == "done"),
        "run completes after reconcile repairs the orphan: {:?}",
        result.outcome
    );
    let events = runner.store().replay(session).await.expect("replay");
    let items = project_input_items(&events, None);
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

/// A notification (an async child completed) enqueued at the same instant as
/// a user interjection must defer: the user message lands in the turn-2 input,
/// the notification stays queued. A notification never jumps ahead of pending
/// user input.
#[tokio::test]
async fn test_notification_defers_to_user() {
    let slot = std::sync::Arc::new(std::sync::OnceLock::<Arc<Runner>>::new());
    let provider = Arc::new(CaptureProvider::with_tool("enqueue_both"));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EnqueueBothTool::new(
        std::sync::Arc::clone(&slot),
        "user interjection",
        "child finished working",
    )));
    let runner = Arc::new(runner_with(
        Arc::clone(&provider) as Arc<dyn houyicoder_api::provider::ModelProvider>,
        tools,
    ));
    slot.set(Arc::clone(&runner)).ok();
    let session = houyicoder_context::SessionId::new();
    let result = runner
        .run(session, "do the task".into())
        .await
        .expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(t) if t == "done"));
    let seen = provider.seen.lock().expect("seen");
    assert!(
        seen[1].contains("user interjection"),
        "turn-2 input carries the queued user message: {}",
        seen[1]
    );
    assert!(
        !seen[1].contains("child finished working"),
        "turn-2 input must NOT carry the deferred notification: {}",
        seen[1]
    );
    // The notification is still pending — it waits for an idle boundary.
    assert_eq!(
        runner.queued_notifications_snapshot(),
        vec!["child finished working".to_string()],
        "notification stays queued while user input was pending"
    );
}

/// A notification enqueued with no pending user input drains at the next turn
/// boundary, so the model learns the child finished. Notifications are not
/// starved indefinitely, only deferred behind user input.
#[tokio::test]
async fn test_notification_drains_when_idle() {
    let slot = std::sync::Arc::new(std::sync::OnceLock::<Arc<Runner>>::new());
    let provider = Arc::new(CaptureProvider::with_tool("enqueue_notification"));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EnqueueNotificationTool::new(
        std::sync::Arc::clone(&slot),
        "child finished working",
    )));
    let runner = Arc::new(runner_with(
        Arc::clone(&provider) as Arc<dyn houyicoder_api::provider::ModelProvider>,
        tools,
    ));
    slot.set(Arc::clone(&runner)).ok();
    let session = houyicoder_context::SessionId::new();
    let result = runner
        .run(session, "do the task".into())
        .await
        .expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(t) if t == "done"));
    let seen = provider.seen.lock().expect("seen");
    assert!(
        seen[1].contains("child finished working"),
        "turn-2 input carries the drained notification: {}",
        seen[1]
    );
    assert!(
        runner.queued_notifications_snapshot().is_empty(),
        "notification drained, queue is empty"
    );
}

struct EnqueueBothTool {
    slot: std::sync::Arc<std::sync::OnceLock<Arc<Runner>>>,
    user_msg: String,
    notification: String,
}
impl EnqueueBothTool {
    fn new(
        slot: std::sync::Arc<std::sync::OnceLock<Arc<Runner>>>,
        user_msg: &str,
        notification: &str,
    ) -> Self {
        Self {
            slot,
            user_msg: user_msg.into(),
            notification: notification.into(),
        }
    }
}
impl Tool for EnqueueBothTool {
    fn name(&self) -> &str {
        "enqueue_both"
    }
    fn description(&self) -> &str {
        "test tool: enqueues a user message + a notification"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: serde_json::Value,
    ) -> houyicoder_async::PFut<'_, Result<serde_json::Value, ToolError>> {
        if let Some(r) = self.slot.get() {
            r.enqueue_input(self.user_msg.clone());
            r.enqueue_notification(self.notification.clone());
        }
        Box::pin(async move { Ok(serde_json::json!({"queued": true})) })
    }
}

struct EnqueueNotificationTool {
    slot: std::sync::Arc<std::sync::OnceLock<Arc<Runner>>>,
    notification: String,
}
impl EnqueueNotificationTool {
    fn new(slot: std::sync::Arc<std::sync::OnceLock<Arc<Runner>>>, notification: &str) -> Self {
        Self {
            slot,
            notification: notification.into(),
        }
    }
}
impl Tool for EnqueueNotificationTool {
    fn name(&self) -> &str {
        "enqueue_notification"
    }
    fn description(&self) -> &str {
        "test tool: enqueues a notification"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: serde_json::Value,
    ) -> houyicoder_async::PFut<'_, Result<serde_json::Value, ToolError>> {
        if let Some(r) = self.slot.get() {
            r.enqueue_notification(self.notification.clone());
        }
        Box::pin(async move { Ok(serde_json::json!({"queued": true})) })
    }
}

/// A clean log (every ToolCall has a matching ToolResult) must not gain any
/// extra ToolResult from the entry reconcile — it is a no-op. Pins idempotence
/// so a healthy session is not mutated.
#[tokio::test]
async fn test_reconcile_noop_clean_log() {
    use houyicoder_context::TurnEventKind;
    use houyicoder_protocol::llm::OutputItem;
    let runner = Arc::new(runner_with(
        Arc::new(crate::provider::test_support::FakeProvider::new(vec![
            CompletionResponse {
                output: vec![OutputItem::Text {
                    text: "done".into(),
                }],
                usage: Usage::default(),
                model: "test".into(),
            },
        ])),
        ToolRegistry::new(),
    ));
    let session = houyicoder_context::SessionId::new();
    // A clean prefix: assistant turn + tool call + its matching result.
    runner
        .store()
        .append(new_event(
            session,
            TurnEventKind::AssistantMessage {
                text: "let me echo".into(),
                thinking: None,
            },
        ))
        .await
        .expect("append assistant");
    runner
        .store()
        .append(new_event(
            session,
            TurnEventKind::ToolCall {
                call_id: "c1".into(),
                tool: "echo".into(),
                input: serde_json::json!({}),
            },
        ))
        .await
        .expect("append tool call");
    runner
        .store()
        .append(new_event(
            session,
            TurnEventKind::tool_result("c1", serde_json::json!({"ok": true})),
        ))
        .await
        .expect("append tool result");
    let before = runner.store().replay(session).await.expect("replay");
    let results_before = before
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::ToolResult { .. }))
        .count();
    runner.run(session, "next".into()).await.expect("run");
    let after = runner.store().replay(session).await.expect("replay");
    let results_after = after
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::ToolResult { .. }))
        .count();
    assert_eq!(
        results_after, results_before,
        "reconcile must not append any ToolResult on a clean log"
    );
}
