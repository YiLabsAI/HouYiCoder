//! Runtime fire-point tests: PreToolUse actually fires during a runner agent
//! run (not the make-check comment gate), a Deny verdict blocks the tool, the
//! model sees the blocked result losslessly, and the tool never executes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_context::{SessionId, TurnEvent, TurnEventKind};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};

use crate::agent::hook::registry::HookRegistry;
use crate::agent::hook::{Hook, HookContext, HookError, HookEvent, HookSource, HookVerdict};
use crate::agent::tests::runner_with;
use crate::agent::{RunOutcome, ToolRegistry};
use crate::provider::test_support::FakeProvider;

/// A tool that records each execution to a shared counter. Does not require
/// approval, so it travels the exec path where PreToolUse fires.
struct RecordingTool {
    name: String,
    ran: Arc<AtomicU8>,
}
impl RecordingTool {
    fn new(ran: Arc<AtomicU8>) -> Self {
        Self {
            name: "recordable".into(),
            ran,
        }
    }
}
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "a tool that records execution"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        input: serde_json::Value,
    ) -> houyicoder_async::PFut<'_, Result<serde_json::Value, ToolError>> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(serde_json::json!({"ran": true, "input": input})) })
    }
}

/// A hook that denies the recordable tool on PreToolUse, allows everything
/// else. Project-sourced (the trust state defaults to Trusted so it fires).
struct DenyRecordableHook;
impl Hook for DenyRecordableHook {
    fn name(&self) -> &str {
        "deny-recordable"
    }
    fn events(&self) -> &[HookEvent] {
        &[HookEvent::PreToolUse]
    }
    fn evaluate(&self, ctx: &HookContext) -> Result<HookVerdict, HookError> {
        match &ctx.payload {
            crate::agent::hook::HookPayload::PreToolUse { tool_name, .. }
                if tool_name == "recordable" =>
            {
                Ok(HookVerdict::Deny(
                    "recordable is off-limits in tests".into(),
                ))
            }
            _ => Ok(HookVerdict::Allow),
        }
    }
    fn source(&self) -> HookSource {
        HookSource::Project
    }
}

/// A hook that observes (non-blocking) on PreToolUse + PostToolUse, never
/// blocks. The tool executes; the observations are recorded. Covers the
/// Allow/Observe keep-path through arbitrate + the full PostToolUse payload +
/// dispatch + record_hook_observations branches.
struct ObserveRecordableHook;
impl Hook for ObserveRecordableHook {
    fn name(&self) -> &str {
        "observe-recordable"
    }
    fn events(&self) -> &[HookEvent] {
        &[HookEvent::PreToolUse, HookEvent::PostToolUse]
    }
    fn evaluate(&self, ctx: &HookContext) -> Result<HookVerdict, HookError> {
        match &ctx.payload {
            crate::agent::hook::HookPayload::PreToolUse { tool_name, .. }
            | crate::agent::hook::HookPayload::PostToolUse { tool_name, .. }
                if tool_name == "recordable" =>
            {
                Ok(HookVerdict::Observe("recordable observed".into()))
            }
            _ => Ok(HookVerdict::Allow),
        }
    }
    fn source(&self) -> HookSource {
        HookSource::Project
    }
}

/// Build a runner whose first scripted response calls the recordable tool,
/// whose second is final text, and whose hook registry denies the recordable
/// tool on PreToolUse. Returns (runner, session, ran-counter).
fn deny_runner() -> (crate::agent::Runner, SessionId, Arc<AtomicU8>) {
    let ran = Arc::new(AtomicU8::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(RecordingTool::new(ran.clone())));
    let first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "call_1".into(),
            name: "recordable".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::new(vec![first, second]));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(DenyRecordableHook));
    let runner = runner_with(provider, tools).with_hooks(Arc::new(registry));
    let session = SessionId::new();
    (runner, session, ran)
}

#[tokio::test]
async fn test_pre_tool_use_deny() {
    let (runner, session, ran) = deny_runner();
    let result = runner.run(session, "go".into()).await.expect("run");
    let text = match result.outcome {
        RunOutcome::FinalOutput(t) => t,
        other => panic!("expected final output, got {other:?}"),
    };
    assert_eq!(text, "done");
    // The tool never executed: PreToolUse Deny removed it from the exec queue
    // + appended a synthetic blocked result before execute could run.
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "denied tool must not execute"
    );
    // The model saw the blocked result losslessly (carried by call_id).
    let blocked = runner
        .store()
        .trajectory_snapshot(session)
        .iter()
        .any(|ev| matches!(ev.kind, TurnEventKind::ToolResult { ref output, .. } if output.to_string().contains("blocked by hook")));
    assert!(
        blocked,
        "the denied tool_result carries the hook block reason"
    );
}

#[tokio::test]
async fn test_hook_deny_records_signal() {
    // The Deny lands a durable HookSignal attributed to the hook (with its
    // name + reason) so the audit trail + ExPeL see WHO blocked WHAT + why —
    // not just the model-visible synthetic result. The model sees the block
    // losslessly; the trajectory sees the verdict + the hook that issued it.
    let (runner, session, _ran) = deny_runner();
    runner.run(session, "go".into()).await.expect("run");
    let snap = runner.store().trajectory_snapshot(session);
    let signals: Vec<&TurnEvent> = snap
        .iter()
        .filter(|ev| matches!(ev.kind, TurnEventKind::HookSignal { .. }))
        .collect();
    let deny = signals.iter().find_map(|ev| match &ev.kind {
        TurnEventKind::HookSignal {
            verdict,
            hook_name,
            reason,
            tool_name,
            error,
            ..
        } if *verdict == houyicoder_context::HookVerdictKind::Deny => {
            Some((hook_name.clone(), reason.clone(), tool_name.clone(), *error))
        }
        _ => None,
    });
    let (hook_name, reason, tool_name, error) = deny.expect("a Deny HookSignal landed");
    assert_eq!(hook_name, "deny-recordable");
    assert!(
        reason.contains("off-limits"),
        "reason carries the hook's denial text, got: {reason}"
    );
    assert_eq!(tool_name.as_deref(), Some("recordable"));
    // A deliberate Deny is NOT a fault — error is None (the two are
    // orthogonal: verdict = effective result, error = cause).
    assert!(error.is_none(), "policy Deny must not set an error kind");
}

#[tokio::test]
async fn test_pre_tool_use_observe() {
    // An Observe verdict is non-blocking: the tool executes, PostToolUse
    // fires after, and the observation is recorded. Covers the Allow/Observe
    // keep-path + the full PostToolUse payload + dispatch + record branches.
    let ran = Arc::new(AtomicU8::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(RecordingTool::new(ran.clone())));
    let first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "call_1".into(),
            name: "recordable".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::new(vec![first, second]));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(ObserveRecordableHook));
    let runner = runner_with(provider, tools).with_hooks(Arc::new(registry));
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    // Observe does not block: the tool executed once.
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "observe verdict must not block execution"
    );
    // The tool result landed in the trajectory (the run is lossless).
    let result_landed = runner
        .store()
        .trajectory_snapshot(session)
        .iter()
        .any(|ev| matches!(ev.kind, TurnEventKind::ToolResult { ref output, .. } if output.to_string().contains("ran")));
    assert!(result_landed, "the executed tool_result lands in the log");
}

#[tokio::test]
async fn test_no_registry_no_fire() {
    let ran = Arc::new(AtomicU8::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(RecordingTool::new(ran.clone())));
    let first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "call_1".into(),
            name: "recordable".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::new(vec![first, second]));
    let runner = runner_with(provider, tools);
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "tool executes when no hook denies it"
    );
    assert!(!runner.hooks_wired(), "no registry wired on a plain runner");
}

/// A hook that returns a Feedback verdict on PreToolUse for the recordable
/// tool. Feedback is non-terminal: the tool is blocked + the model sees the
/// self-correction signal, can retry with adjusted input.
struct FeedbackRecordableHook;
impl Hook for FeedbackRecordableHook {
    fn name(&self) -> &str {
        "feedback-recordable"
    }
    fn events(&self) -> &[HookEvent] {
        &[HookEvent::PreToolUse]
    }
    fn evaluate(&self, ctx: &HookContext) -> Result<HookVerdict, HookError> {
        match &ctx.payload {
            crate::agent::hook::HookPayload::PreToolUse { tool_name, .. }
                if tool_name == "recordable" =>
            {
                Ok(HookVerdict::Feedback("rewrite the recordable call".into()))
            }
            _ => Ok(HookVerdict::Allow),
        }
    }
    fn source(&self) -> HookSource {
        HookSource::Project
    }
}

/// A tool that always fails execution, so PostToolUseFailure fires.
struct FailingTool;
impl Tool for FailingTool {
    fn name(&self) -> &str {
        "recordable"
    }
    fn description(&self) -> &str {
        "a tool that always errors"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: serde_json::Value,
    ) -> houyicoder_async::PFut<'_, Result<serde_json::Value, ToolError>> {
        Box::pin(async move { Err(ToolError::Failed("always fails".into())) })
    }
}

#[tokio::test]
async fn test_pre_tool_use_feedback() {
    // Feedback surfaces a self-correction signal: the tool is blocked + the
    // model sees the feedback reason, can retry with adjusted input. Covers
    // the Feedback arm + hook_feedback_json.
    let ran = Arc::new(AtomicU8::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(RecordingTool::new(ran.clone())));
    let first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "call_1".into(),
            name: "recordable".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::new(vec![first, second]));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(FeedbackRecordableHook));
    let runner = runner_with(provider, tools).with_hooks(Arc::new(registry));
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "feedback blocks the call, tool does not execute"
    );
    let feedback = runner
        .store()
        .trajectory_snapshot(session)
        .iter()
        .any(|ev| matches!(ev.kind, TurnEventKind::ToolResult { ref output, .. } if output.to_string().contains("hook feedback")));
    assert!(
        feedback,
        "the feedback tool_result carries the self-correction signal"
    );
}

#[tokio::test]
async fn test_post_tool_use_fires() {
    // A tool that errors: PostToolUseFailure fires with the error extracted
    // from the result. Covers the is_error=true payload-building branch.
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(FailingTool));
    let first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "call_1".into(),
            name: "recordable".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::new(vec![first, second]));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(ObserveRecordableHook));
    let runner = runner_with(provider, tools).with_hooks(Arc::new(registry));
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    // The failing tool's error result landed in the trajectory.
    let failed = runner
        .store()
        .trajectory_snapshot(session)
        .iter()
        .any(|ev| matches!(ev.kind, TurnEventKind::ToolResult { ref output, .. } if output.to_string().contains("always fails")));
    assert!(failed, "the failing tool's error result lands losslessly");
}

/// Gap B idempotivity red line: a turn that crashed after the tool executed
/// (result in the durable log) but before the model reply landed. On redrive,
/// the tool must NOT re-execute — the model sees the existing ToolResult in
/// the log and regenerates the reply without re-requesting the tool. A
/// TurnAborted boundary marker lands before the regenerated content.
#[tokio::test]
async fn test_recover_turn_not_reexecute() {
    use houyicoder_context::TurnEventKind;

    let ran = Arc::new(AtomicU8::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(RecordingTool::new(ran.clone())));
    // The provider scripts only the final text — on redrive the model sees
    // the partial turn (user input + tool call + tool result) in the log
    // and generates the reply, not a new tool call.
    let final_text = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider = Arc::new(FakeProvider::new(vec![final_text]));
    let runner = runner_with(provider, tools);
    let session = SessionId::new();

    // Manually append the partial turn: user input, tool call, tool result.
    // This simulates a crash after the tool executed but before the model
    // generated the reply.
    runner
        .append_user_input(session, "go".into())
        .await
        .expect("append user input");
    runner
        .store()
        .append(houyicoder_context::TurnEvent {
            id: houyicoder_context::EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::ToolCall {
                call_id: "toolu_1".into(),
                tool: "recordable".into(),
                input: serde_json::json!({}),
            },
        })
        .await
        .expect("append tool call");
    runner
        .append_tool_result(
            session,
            "toolu_1".into(),
            "",
            serde_json::json!({"ran": true}),
            0,
        )
        .await
        .expect("append tool result");

    // The recording tool was NOT executed by us — we manually appended the
    // result. Counter is 0.
    assert_eq!(ran.load(Ordering::SeqCst), 0, "tool not yet executed");

    // Redrive: the model sees the partial turn + regenerates the reply.
    let result = runner.recover_turn(session).await.expect("redrive");
    assert!(
        matches!(result.outcome, RunOutcome::FinalOutput(_)),
        "redrive completes with a final output"
    );

    // Red line: the tool was NOT re-executed on redrive (counter still 0).
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "tool must NOT re-execute on redrive — its result is durable in the log"
    );

    // Guardrail 1: a TurnAborted boundary marker landed before the
    // regenerated content.
    let has_aborted = runner
        .store()
        .trajectory_snapshot(session)
        .iter()
        .any(|ev| matches!(ev.kind, TurnEventKind::TurnAborted { .. }));
    assert!(
        has_aborted,
        "TurnAborted boundary marker must land before the regenerated turn"
    );
}

/// Idempotivity for a crash during the assistant reply (partial
/// AssistantTextDelta landed but no authoritative AssistantMessage). On
/// recover, the model sees the partial turn and regenerates the full reply.
/// No tool is re-executed because the tool result is already durable.
#[tokio::test]
async fn test_recover_reply_not_reexecute() {
    let ran = Arc::new(AtomicU8::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(RecordingTool::new(ran.clone())));
    // The provider scripts the final text — on recover the model sees the
    // partial turn (user input + tool call + tool result; the streamed delta
    // is transport-only, not in the durable log) and generates the full reply.
    let final_text = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider = Arc::new(FakeProvider::new(vec![final_text]));
    let runner = runner_with(provider, tools);
    let session = SessionId::new();

    // Manually append a partial turn that crashed mid-reply: the tool
    // executed (result durable) and the model started streaming a reply
    // (one delta) but the process died before the authoritative message.
    runner
        .append_user_input(session, "go".into())
        .await
        .expect("append user input");
    runner
        .store()
        .append(houyicoder_context::TurnEvent {
            id: houyicoder_context::EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::ToolCall {
                call_id: "toolu_1".into(),
                tool: "recordable".into(),
                input: serde_json::json!({}),
            },
        })
        .await
        .expect("append tool call");
    runner
        .append_tool_result(
            session,
            "toolu_1".into(),
            "",
            serde_json::json!({"ran": true}),
            0,
        )
        .await
        .expect("append tool result");
    runner
        .store()
        .append(houyicoder_context::TurnEvent {
            id: houyicoder_context::EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantTextDelta { text: "par".into() },
        })
        .await
        .expect("append partial delta");

    assert_eq!(ran.load(Ordering::SeqCst), 0, "tool not yet executed");

    let result = runner.recover_turn(session).await.expect("recover");
    assert!(
        matches!(result.outcome, RunOutcome::FinalOutput(_)),
        "recover completes with a final output"
    );
    // Red line: tool NOT re-executed despite the partial reply crash.
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "tool must NOT re-execute on recover after partial-reply crash"
    );
}

/// A PreToolUse Deny feeds signal B: the memory provider's
/// record_gate_violation counter accumulates for the denied rule. Closes
/// the U7 loop where a rule the agent keeps violating finally accumulates
/// enough violations for the dream to promote it into the always-on carrier.
#[tokio::test]
async fn test_deny_records_gate_violation() {
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// A recording memory that captures every record_gate_violation call
    /// so the test asserts the deny fed signal B without a real sidecar.
    struct ViolationMemory {
        violations: Mutex<Vec<String>>,
    }
    impl houyicoder_api::memory::MemoryProvider for ViolationMemory {
        fn recall(
            &self,
            _q: &str,
            _b: usize,
            _s: &HashSet<String>,
        ) -> Vec<houyicoder_context::MemoryEntry> {
            Vec::new()
        }
        fn add(
            &self,
            _e: houyicoder_context::MemoryEntry,
        ) -> Result<(), houyicoder_context::MemoryError> {
            Ok(())
        }
        fn record_gate_violation(&self, key: &str) {
            self.violations
                .lock()
                .expect("violations")
                .push(key.to_string());
        }
    }

    let memory = Arc::new(ViolationMemory {
        violations: Mutex::new(Vec::new()),
    });

    // A hook that denies the recordable tool with a reason that names the
    // rule (the memory key the dream would promote).
    struct DenyNamedRule;
    impl Hook for DenyNamedRule {
        fn name(&self) -> &str {
            "deny-test-placement"
        }
        fn events(&self) -> &[HookEvent] {
            &[HookEvent::PreToolUse]
        }
        fn evaluate(&self, ctx: &HookContext) -> Result<HookVerdict, HookError> {
            match &ctx.payload {
                crate::agent::hook::HookPayload::PreToolUse { tool_name, .. }
                    if tool_name == "recordable" =>
                {
                    Ok(HookVerdict::Deny("test-placement".into()))
                }
                _ => Ok(HookVerdict::Allow),
            }
        }
        fn source(&self) -> HookSource {
            HookSource::User
        }
    }

    let ran = Arc::new(AtomicU8::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(RecordingTool::new(ran.clone())));
    let first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "call_1".into(),
            name: "recordable".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::new(vec![first, second]));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(DenyNamedRule));
    let runner = runner_with(provider, tools)
        .with_hooks(Arc::new(registry))
        .with_memory(Arc::clone(&memory) as Arc<dyn houyicoder_api::memory::MemoryProvider>);
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    // The deny fired exactly once, feeding signal B with the rule key.
    let violations = memory.violations.lock().expect("violations").clone();
    assert_eq!(violations.len(), 1, "exactly one deny -> one violation");
    assert_eq!(
        violations[0], "test-placement",
        "violation recorded against the deny reason (the rule key)"
    );
}

/// LiveProgressSink forwards a progress(elapsed, None) call to the live
/// stream as a ToolProgress event carrying the sink's call_id + the elapsed
/// seconds. None when no live sink is wired (no-op). Pins the runner→host
/// forwarding side of the bash-elapsed channel.
#[test]
fn test_live_progress_sink_forwards() {
    use super::LiveProgressSink;
    use houyicoder_api::live::LiveEvent;
    use houyicoder_api::progress::ProgressSink;
    use std::sync::Mutex;

    let collected = Arc::new(Mutex::new(Vec::<LiveEvent>::new()));
    let sink_live = Arc::new({
        let collected = Arc::clone(&collected);
        move |ev: &LiveEvent| collected.lock().expect("collected").push(ev.clone())
    }) as Arc<dyn Fn(&LiveEvent) + Send + Sync>;
    let live = Arc::new(LiveProgressSink::new("c1".into(), Some(sink_live)));
    live.progress(12, None);
    let got = collected.lock().expect("collected").clone();
    assert_eq!(got.len(), 1, "one event forwarded");
    match &got[0] {
        LiveEvent::ToolProgress {
            call_id,
            elapsed_secs,
            ..
        } => {
            assert_eq!(call_id, "c1");
            assert_eq!(*elapsed_secs, 12);
        }
        _ => panic!("expected ToolProgress, got {:?}", got[0]),
    }

    // No live sink wired: no-op (must not panic, must not forward).
    let none_sink = LiveProgressSink::new("c2".into(), None);
    none_sink.progress(5, None);
}
