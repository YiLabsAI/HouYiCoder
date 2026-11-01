//! Shell checks skipped after an edit in the same turn did not complete.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use houyicoder_api::provider::ModelProvider;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::ToolRegistry;
use crate::agent::hook::registry::HookRegistry;
use crate::agent::hook::{
    Hook, HookContext, HookError, HookEvent, HookPayload, HookSource, HookVerdict,
};
use crate::agent::tests::runner_with;
use crate::agent::turn::fallback::SKIPPED_AFTER_EDIT;
use crate::agent::turn::resolve::ToolCallPlan;
use crate::agent::{ApprovalDecision, RunOutcome, Runner};
use crate::provider::test_support::FakeProvider;

/// What the file stub answers, and a counter of the runs.
#[derive(Clone, Copy)]
enum StubOutcome {
    Succeeds,
    Fails,
    Interrupted,
}

struct StubFileTool {
    name: &'static str,
    outcome: StubOutcome,
    ask: bool,
    ran: Arc<AtomicU8>,
}

impl StubFileTool {
    fn new(name: &'static str, outcome: StubOutcome) -> (Arc<Self>, Arc<AtomicU8>) {
        Self::build(name, outcome, false)
    }

    /// A file tool whose call needs a human verdict before it runs.
    fn asking(name: &'static str) -> (Arc<Self>, Arc<AtomicU8>) {
        Self::build(name, StubOutcome::Succeeds, true)
    }

    fn build(name: &'static str, outcome: StubOutcome, ask: bool) -> (Arc<Self>, Arc<AtomicU8>) {
        let ran = Arc::new(AtomicU8::new(0));
        (
            Arc::new(Self {
                name,
                outcome,
                ask,
                ran: ran.clone(),
            }),
            ran,
        )
    }
}

impl Tool for StubFileTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "stub file tool"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        let outcome = self.outcome;
        Box::pin(async move {
            match outcome {
                StubOutcome::Succeeds => Ok(serde_json::json!({"success": true})),
                StubOutcome::Fails => Err(ToolError::InvalidInput("old_string not found".into())),
                StubOutcome::Interrupted => {
                    Ok(serde_json::json!({"error": "interrupted by user", "not_run": true}))
                }
            }
        })
    }

    fn requires_approval(&self) -> bool {
        self.ask
    }
}

/// A file tool with one required argument: a call that omits it cannot run,
/// so the host answers it with the schema error instead of executing.
struct RequiredArgEdit {
    ran: Arc<AtomicU8>,
}

impl Tool for RequiredArgEdit {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "stub file tool"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {"path": {"type": "string"}},
        })
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(serde_json::json!({"success": true})) })
    }
}

/// A shell stub that records every command it runs.
struct ShellStub {
    asking: bool,
    commands: Mutex<Vec<String>>,
}

impl ShellStub {
    fn new() -> Arc<Self> {
        Self::build(false)
    }

    /// A shell whose call needs a human verdict before it runs.
    fn asking() -> Arc<Self> {
        Self::build(true)
    }

    fn build(asking: bool) -> Arc<Self> {
        Arc::new(Self {
            asking,
            commands: Mutex::new(Vec::new()),
        })
    }

    fn ran(&self) -> Vec<String> {
        self.commands.lock().expect("shell stub lock").clone()
    }
}

impl Tool for ShellStub {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "stub shell"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.commands
            .lock()
            .expect("shell stub lock")
            .push(command.clone());
        Box::pin(async move { Ok(serde_json::json!({"success": true, "command": command})) })
    }

    fn requires_approval(&self) -> bool {
        self.asking
    }
}

/// A hook that refuses the edit tool before it runs.
struct DenyEditHook;

impl Hook for DenyEditHook {
    fn name(&self) -> &str {
        "deny-edit"
    }
    fn events(&self) -> &[HookEvent] {
        &[HookEvent::PreToolUse]
    }
    fn evaluate(&self, ctx: &HookContext) -> Result<HookVerdict, HookError> {
        match &ctx.payload {
            HookPayload::PreToolUse { tool_name, .. } if tool_name == "edit" => {
                Ok(HookVerdict::Deny("source edit refused in tests".into()))
            }
            _ => Ok(HookVerdict::Allow),
        }
    }
    fn source(&self) -> HookSource {
        HookSource::Project
    }
}

fn shell_command(command: &str) -> Value {
    serde_json::json!({ "command": command })
}

fn plan(id: &str, tool: Arc<dyn Tool>, input: Value) -> ToolCallPlan {
    ToolCallPlan::new(id.to_string(), tool, input)
}

fn runner() -> Runner {
    runner_with(Arc::new(FakeProvider::text("done")), ToolRegistry::new())
}

fn logged_skip(runner: &Runner, session: SessionId, call_id: &str) -> bool {
    runner.store().trajectory_tail(session).iter().any(|entry| {
        matches!(&entry.event, SessionEvent::ToolResult { call_id: id, output, .. }
            if id == call_id
                && output["error"] == SKIPPED_AFTER_EDIT
                && output["not_run"] == true)
    })
}

#[tokio::test]
async fn test_failed_edit_skips_bash() {
    let runner = runner();
    let session = SessionId::new();
    let (edit, edit_ran) = StubFileTool::new("edit", StubOutcome::Fails);
    let shell = ShellStub::new();
    let plans = vec![
        plan("e1", edit, serde_json::json!({})),
        plan("b1", shell.clone(), shell_command("cargo test")),
    ];

    let results = runner
        .execute_partitioned(session, &plans, &CancellationToken::new())
        .await
        .expect("dispatch");

    assert_eq!(edit_ran.load(Ordering::SeqCst), 1, "the edit was attempted");
    assert!(shell.ran().is_empty(), "the check did not run");
    let skipped = results
        .iter()
        .find(|(id, _)| id == "b1")
        .expect("the check is answered");
    assert_eq!(skipped.1["error"], SKIPPED_AFTER_EDIT);
    assert!(logged_skip(&runner, session, "b1"), "the answer is durable");
}

#[tokio::test]
async fn test_edit_then_bash_runs() {
    let runner = runner();
    let session = SessionId::new();
    let (edit, edit_ran) = StubFileTool::new("edit", StubOutcome::Succeeds);
    let shell = ShellStub::new();
    let plans = vec![
        plan("e1", edit, serde_json::json!({})),
        plan("b1", shell.clone(), shell_command("cargo test")),
    ];

    let results = runner
        .execute_partitioned(session, &plans, &CancellationToken::new())
        .await
        .expect("dispatch");

    assert_eq!(edit_ran.load(Ordering::SeqCst), 1, "the edit ran");
    assert_eq!(shell.ran(), ["cargo test"], "the check ran after the edit");
    let bash = results
        .iter()
        .find(|(id, _)| id == "b1")
        .expect("the check is answered");
    assert!(bash.1.get("error").is_none(), "the check succeeded");
}

#[tokio::test]
async fn test_failed_read_keeps_bash() {
    let runner = runner();
    let session = SessionId::new();
    let (read, _) = StubFileTool::new("read", StubOutcome::Fails);
    let shell = ShellStub::new();
    let plans = vec![
        plan("r1", read, serde_json::json!({})),
        plan("b1", shell.clone(), shell_command("git status")),
    ];

    runner
        .execute_partitioned(session, &plans, &CancellationToken::new())
        .await
        .expect("dispatch");

    assert_eq!(
        shell.ran(),
        ["git status"],
        "a failed read does not cancel later work"
    );
}

/// A call the run cancelled under is not a failed edit: its callers are
/// already aborting, so the later check keeps its own answer instead of being
/// reported as skipped.
#[tokio::test]
async fn test_interrupted_edit_keeps_bash() {
    let runner = runner();
    let session = SessionId::new();
    let (edit, edit_ran) = StubFileTool::new("edit", StubOutcome::Interrupted);
    let shell = ShellStub::new();
    let plans = vec![
        plan("e1", edit, serde_json::json!({})),
        plan("b1", shell.clone(), shell_command("cargo test")),
    ];

    let results = runner
        .execute_partitioned(session, &plans, &CancellationToken::new())
        .await
        .expect("dispatch");

    assert_eq!(edit_ran.load(Ordering::SeqCst), 1, "the edit was attempted");
    assert_eq!(shell.ran(), ["cargo test"], "the check still runs");
    let bash = results
        .iter()
        .find(|(id, _)| id == "b1")
        .expect("the check is answered");
    assert!(
        bash.1.get("error").is_none(),
        "a cancelled edit does not skip the check"
    );
}

#[tokio::test]
async fn test_bash_before_edit_runs() {
    let runner = runner();
    let session = SessionId::new();
    let (edit, _) = StubFileTool::new("edit", StubOutcome::Fails);
    let shell = ShellStub::new();
    let plans = vec![
        plan("b1", shell.clone(), shell_command("git status")),
        plan("e1", edit, serde_json::json!({})),
        plan("b2", shell.clone(), shell_command("cargo test")),
    ];

    let results = runner
        .execute_partitioned(session, &plans, &CancellationToken::new())
        .await
        .expect("dispatch");

    assert_eq!(
        shell.ran(),
        ["git status"],
        "only the check after the edit is withheld"
    );
    let skipped = results
        .iter()
        .find(|(id, _)| id == "b2")
        .expect("the later check is answered");
    assert_eq!(skipped.1["error"], SKIPPED_AFTER_EDIT);
}

/// An edit waiting for a human verdict has not run either, so the check that
/// follows it in the same response must not run ahead of the approval.
#[tokio::test]
async fn test_pending_edit_skips_bash() {
    let (edit, edit_ran) = StubFileTool::asking("edit");
    let shell = ShellStub::new();
    let mut tools = ToolRegistry::new();
    tools.register(edit);
    tools.register(shell.clone());
    let first = CompletionResponse {
        output: vec![
            OutputItem::ToolCall {
                id: "call_edit".into(),
                name: "edit".into(),
                input: serde_json::json!({}),
            },
            OutputItem::ToolCall {
                id: "call_bash".into(),
                name: "bash".into(),
                input: shell_command("cargo test"),
            },
        ],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![first]));
    let runner = runner_with(provider, tools);
    let session = SessionId::new();

    let result = runner.run(session, "go".into()).await.expect("run");

    assert!(
        matches!(result.outcome, RunOutcome::Interruption(_)),
        "the edit waits for a verdict"
    );
    assert_eq!(edit_ran.load(Ordering::SeqCst), 0, "the edit never ran");
    assert!(shell.ran().is_empty(), "the check did not run first");
    assert!(
        logged_skip(&runner, session, "call_bash"),
        "the withheld check keeps a durable answer"
    );
}

/// A call missing its required argument cannot run, so the check after it is
/// answered as skipped instead of running on the unedited source.
#[tokio::test]
async fn test_unrunnable_edit_skips_bash() {
    let ran = Arc::new(AtomicU8::new(0));
    let edit = Arc::new(RequiredArgEdit { ran: ran.clone() });
    let shell = ShellStub::new();
    let mut tools = ToolRegistry::new();
    tools.register(edit);
    tools.register(shell.clone());
    let first = CompletionResponse {
        output: vec![
            OutputItem::ToolCall {
                id: "call_edit".into(),
                name: "edit".into(),
                input: serde_json::json!({}),
            },
            OutputItem::ToolCall {
                id: "call_bash".into(),
                name: "bash".into(),
                input: shell_command("cargo test"),
            },
        ],
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
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![first, second]));
    let runner = runner_with(provider, tools);
    let session = SessionId::new();

    runner.run(session, "go".into()).await.expect("run");

    assert_eq!(ran.load(Ordering::SeqCst), 0, "the edit never ran");
    assert!(shell.ran().is_empty(), "the check did not run");
    assert!(
        logged_skip(&runner, session, "call_bash"),
        "the withheld check keeps a durable answer"
    );
}

/// The PreToolUse refusal path: every call of the turn is gated before any
/// runs, so the withheld check still has to keep its result.
#[tokio::test]
async fn test_blocked_edit_skips_bash() {
    let (edit, edit_ran) = StubFileTool::new("edit", StubOutcome::Succeeds);
    let shell = ShellStub::new();
    let mut tools = ToolRegistry::new();
    tools.register(edit);
    tools.register(shell.clone());
    let first = CompletionResponse {
        output: vec![
            OutputItem::ToolCall {
                id: "call_edit".into(),
                name: "edit".into(),
                input: serde_json::json!({}),
            },
            OutputItem::ToolCall {
                id: "call_bash".into(),
                name: "bash".into(),
                input: shell_command("cargo test"),
            },
        ],
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
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![first, second]));
    let registry = HookRegistry::new();
    registry.register(Arc::new(DenyEditHook));
    let runner = runner_with(provider, tools).with_hooks(Arc::new(registry));
    let session = SessionId::new();

    runner.run(session, "go".into()).await.expect("run");

    assert_eq!(edit_ran.load(Ordering::SeqCst), 0, "the edit never ran");
    assert!(shell.ran().is_empty(), "the check did not run");
    assert!(
        logged_skip(&runner, session, "call_bash"),
        "the withheld check keeps a durable answer"
    );
}

/// Both the edit and its check need a human verdict. Both cards are raised:
/// the check is not answered while its edit is undecided, so approving the
/// edit releases the check on the same resume instead of costing another
/// model response.
#[tokio::test]
async fn test_pending_edit_raises_both() {
    let (edit, edit_ran) = StubFileTool::asking("edit");
    let shell = ShellStub::asking();
    let mut tools = ToolRegistry::new();
    tools.register(edit);
    tools.register(shell.clone());
    let first = CompletionResponse {
        output: vec![
            OutputItem::ToolCall {
                id: "call_edit".into(),
                name: "edit".into(),
                input: serde_json::json!({}),
            },
            OutputItem::ToolCall {
                id: "call_bash".into(),
                name: "bash".into(),
                input: shell_command("cargo test"),
            },
        ],
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
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![first, second]));
    let runner = runner_with(provider, tools);
    let session = SessionId::new();

    let result = runner.run(session, "go".into()).await.expect("run");

    let RunOutcome::Interruption(requests) = &result.outcome else {
        panic!("both calls wait for a verdict: {:?}", result.outcome);
    };
    assert_eq!(requests.len(), 2, "both calls are put to a human");
    assert_eq!(requests[0].call_id, "call_edit");
    assert_eq!(requests[1].call_id, "call_bash");
    assert_eq!(edit_ran.load(Ordering::SeqCst), 0, "the edit never ran");
    assert!(shell.ran().is_empty(), "the check did not run first");
    assert!(
        !logged_skip(&runner, session, "call_bash"),
        "the check keeps no verdict while its edit is undecided"
    );

    let resumed = runner
        .resume(
            session,
            &[
                ApprovalDecision::approve("call_edit"),
                ApprovalDecision::approve("call_bash"),
            ],
        )
        .await
        .expect("resume");

    assert!(
        matches!(resumed.outcome, RunOutcome::FinalOutput(_)),
        "the turn answers: {:?}",
        resumed.outcome
    );
    assert_eq!(
        shell.ran(),
        ["cargo test"],
        "the check runs on its edit's success"
    );
}

/// A call naming a tool the host does not resolve never ran. The check the
/// model sent after it must not run either: the model meant an edit, and the
/// source is unchanged.
#[tokio::test]
async fn test_unknown_edit_skips_bash() {
    let shell = ShellStub::new();
    let mut tools = ToolRegistry::new();
    tools.register(shell.clone());
    let first = CompletionResponse {
        output: vec![
            OutputItem::ToolCall {
                id: "call_edit".into(),
                name: "Edit".into(),
                input: serde_json::json!({}),
            },
            OutputItem::ToolCall {
                id: "call_bash".into(),
                name: "bash".into(),
                input: shell_command("cargo test"),
            },
        ],
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
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![first, second]));
    let runner = runner_with(provider, tools);
    let session = SessionId::new();

    runner.run(session, "go".into()).await.expect("run");

    assert!(shell.ran().is_empty(), "the check did not run");
    assert!(
        logged_skip(&runner, session, "call_bash"),
        "the withheld check keeps a durable answer"
    );
}

/// The check waits for a human verdict and the edit beside it fails while
/// running. The failure is discovered only after the check was separated, and
/// it still withdraws the check before the card is raised.
#[tokio::test]
async fn test_failed_edit_drops_check() {
    let (edit, edit_ran) = StubFileTool::new("edit", StubOutcome::Fails);
    let shell = ShellStub::asking();
    let mut tools = ToolRegistry::new();
    tools.register(edit);
    tools.register(shell.clone());
    let first = CompletionResponse {
        output: vec![
            OutputItem::ToolCall {
                id: "call_edit".into(),
                name: "edit".into(),
                input: serde_json::json!({}),
            },
            OutputItem::ToolCall {
                id: "call_bash".into(),
                name: "bash".into(),
                input: shell_command("cargo test"),
            },
        ],
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
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![first, second]));
    let runner = runner_with(provider, tools);
    let session = SessionId::new();

    let result = runner.run(session, "go".into()).await.expect("run");

    assert_eq!(edit_ran.load(Ordering::SeqCst), 1, "the edit was attempted");
    assert!(shell.ran().is_empty(), "the check did not run");
    assert!(
        !matches!(result.outcome, RunOutcome::Interruption(_)),
        "the withdrawn check raises no card"
    );
    assert!(
        logged_skip(&runner, session, "call_bash"),
        "the withheld check keeps a durable answer"
    );
}
