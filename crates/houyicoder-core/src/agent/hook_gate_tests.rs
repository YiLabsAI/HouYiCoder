//! Pre-tool-use gate over the calls a turn emitted, a call awaiting the
//! user's approval included. A rule that refuses such a call outranks the
//! decision on it, so nobody is asked to approve what the rule already
//! refused; a rule that allows it is not a decision, so the call still awaits
//! the user's consent.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};

use crate::agent::ToolRegistry;
use crate::agent::hook::{
    Hook, HookContext, HookError, HookEvent, HookRegistry, HookSource, HookVerdict,
};
use crate::agent::runner_tests::{GuardedTool, runner_with};
use crate::agent::{ApprovalDecision, RunOutcome};
use crate::provider::test_support::FakeProvider;

/// A project rule that refuses every call about to run, standing in for the
/// deny hook of a real project.
struct RefuseAll;
impl Hook for RefuseAll {
    fn name(&self) -> &str {
        "refuse-all"
    }
    fn events(&self) -> &[HookEvent] {
        &[HookEvent::PreToolUse]
    }
    fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
        Ok(HookVerdict::Deny("project rule: not this path".into()))
    }
    fn source(&self) -> HookSource {
        HookSource::Project
    }
}

/// A project rule that lets every call through, the counterpart to RefuseAll.
struct AllowAll;
impl Hook for AllowAll {
    fn name(&self) -> &str {
        "allow-all"
    }
    fn events(&self) -> &[HookEvent] {
        &[HookEvent::PreToolUse]
    }
    fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
        Ok(HookVerdict::Allow)
    }
    fn source(&self) -> HookSource {
        HookSource::Project
    }
}

/// A project rule that counts the PostToolUse dispatches it sees.
struct CountPostUse {
    fired: Arc<AtomicUsize>,
}
impl Hook for CountPostUse {
    fn name(&self) -> &str {
        "count-post-use"
    }
    fn events(&self) -> &[HookEvent] {
        &[HookEvent::PostToolUse]
    }
    fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
        self.fired.fetch_add(1, Ordering::SeqCst);
        Ok(HookVerdict::Observe("call finished".into()))
    }
    fn source(&self) -> HookSource {
        HookSource::Project
    }
}

/// One turn asking for a call the user must approve, then a turn that answers.
fn call_then_answer() -> Vec<CompletionResponse> {
    vec![
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
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ]
}

fn runner_with_rule(hook: Arc<dyn Hook>) -> crate::agent::Runner {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let reg = HookRegistry::new();
    reg.register(hook);
    runner_with(Arc::new(FakeProvider::new(call_then_answer())), tools).with_hooks(Arc::new(reg))
}

/// The approved call runs through the loop's own dispatch, so it fires
/// PostToolUse and lands its durable result. While the approval path ran the
/// tool on its own, no post-use hook saw an approved call.
#[tokio::test]
async fn test_approved_call_fires_hooks() {
    let fired = Arc::new(AtomicUsize::new(0));
    let runner = runner_with_rule(Arc::new(CountPostUse {
        fired: fired.clone(),
    }));
    let session = SessionId::new();
    let paused = runner.run(session, "go".into()).await.expect("run ok");
    let approvals = match paused.outcome {
        RunOutcome::Interruption(approvals) => approvals,
        other => panic!("expected the call to await approval, got {other:?}"),
    };
    assert_eq!(
        fired.load(Ordering::SeqCst),
        0,
        "the call has not run before a decision"
    );
    let decisions: Vec<ApprovalDecision> = approvals
        .iter()
        .map(|req| ApprovalDecision::approve(&req.call_id))
        .collect();
    let done = runner.resume(session, &decisions).await.expect("resume ok");
    assert!(
        matches!(done.outcome, RunOutcome::FinalOutput(_)),
        "the approved call runs and the turn answers: {:?}",
        done.outcome
    );
    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "the approved call fires PostToolUse once"
    );
}

async fn result_for(
    runner: &crate::agent::Runner,
    session: SessionId,
    want: &str,
) -> Option<serde_json::Value> {
    let events = runner.store().replay(session).await.expect("replay");
    events.iter().find_map(|e| match &e.event {
        SessionEvent::ToolResult {
            call_id, output, ..
        } if call_id == want => Some(output.clone()),
        _ => None,
    })
}

/// The refusal wins over the approval: the call is blocked with the rule's
/// reason, no approval is raised, and the run reaches its answer. Before the
/// gate read approval-requiring calls, this call went straight to the
/// approval card and the rule never saw it.
#[tokio::test]
async fn test_deny_outranks_approval() {
    let runner = runner_with_rule(Arc::new(RefuseAll));
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run ok");
    assert!(
        matches!(result.outcome, RunOutcome::FinalOutput(_)),
        "a refused call does not pause the run for approval: {:?}",
        result.outcome
    );
    let output = result_for(&runner, session, "c1")
        .await
        .expect("the refused call lands a result");
    assert!(
        output
            .get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|e| e.contains("not this path")),
        "the model sees the rule's reason: {output:?}"
    );
}

/// The counterpart: a rule that allows the call is not the user's consent.
/// The call still awaits a decision and does not run before one.
#[tokio::test]
async fn test_allow_keeps_approval() {
    let runner = runner_with_rule(Arc::new(AllowAll));
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run ok");
    match result.outcome {
        RunOutcome::Interruption(approvals) => {
            assert_eq!(approvals.len(), 1, "the call still awaits a decision");
            assert_eq!(approvals[0].call_id, "c1");
        }
        other => panic!("expected the call to await approval, got {other:?}"),
    }
    assert!(
        result_for(&runner, session, "c1").await.is_none(),
        "an allowed call must not run before a decision"
    );
}
