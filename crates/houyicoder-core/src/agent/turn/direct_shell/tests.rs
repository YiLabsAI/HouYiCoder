//! Direct shell: the command runs without a model turn, its result and its
//! refusals land in the session log, and an abort stops it.

use std::future::pending;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use houyicoder_api::session::SessionLog;
use houyicoder_api::session::TrajectoryHead;
use houyicoder_api::session::TrajectoryPage;
use houyicoder_api::session::TrajectoryPosition;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, ContextSnapshot, EventId,
    SessionEvent, SessionId, SessionLogEntry,
};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_protocol::tool::is_user_shell_call_id;
use houyicoder_session::SessionStore;
use serde_json::Value;
use serde_json::json;
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::agent::RunOutcome;
use crate::agent::Runner;
use crate::agent::ToolRegistry;
use crate::agent::hook::{
    Hook, HookContext, HookError, HookEvent, HookRegistry, HookSource, HookVerdict,
};
use crate::agent::runner_config::RunnerConfig;
use crate::agent::tests::runner_with;
use crate::provider::test_support::FakeProvider;

/// Stands in for the sandbox shell: answers what it was asked to run and counts
/// the times it ran.
struct EchoShell {
    calls: Arc<AtomicUsize>,
    failure: Option<String>,
}

impl Tool for EchoShell {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "stands in for the sandbox shell"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let calls = self.calls.clone();
        let failure = self.failure.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            if let Some(reason) = failure {
                return Err(ToolError::Failed(reason));
            }
            let command = input.get("command").and_then(Value::as_str).unwrap_or("");
            Ok(json!({ "stdout": format!("ran {command}"), "success": true }))
        })
    }
}

/// Starts, then never returns: only the cancel race can end the call.
struct HangingShell {
    started: Mutex<Option<oneshot::Sender<()>>>,
}

impl Tool for HangingShell {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "stands in for a shell command that does not finish"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let started = self.started.lock().expect("started slot").take();
        Box::pin(async move {
            if let Some(report) = started {
                let _ = report.send(());
            }
            pending::<()>().await;
            Ok(Value::Null)
        })
    }
}

/// A log whose first append holds until the test releases it, so a cancel can
/// be issued inside the window between the command being recorded and the
/// command starting. Every later write goes straight through.
struct SlowFirstAppend {
    inner: Arc<dyn SessionLog>,
    reached: Mutex<Option<oneshot::Sender<()>>>,
    resume: Mutex<Option<oneshot::Receiver<()>>>,
}

impl SessionLog for SlowFirstAppend {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        let inner = Arc::clone(&self.inner);
        let reached = self.reached.lock().expect("reached slot").take();
        let resume = self.resume.lock().expect("resume slot").take();
        Box::pin(async move {
            if let (Some(report), Some(wait)) = (reached, resume) {
                let _ = report.send(());
                let _ = wait.await;
            }
            inner.append(event).await
        })
    }

    fn append_if<'a>(
        &'a self,
        event: SessionLogEntry,
        allow: &'a (dyn Fn(&[SessionLogEntry]) -> bool + Send + Sync),
    ) -> PFut<'a, Result<Option<EventId>, ContextError>> {
        self.inner.append_if(event, allow)
    }

    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.replay(session)
    }

    fn current_view(&self, session: SessionId) -> PFut<'_, Result<ContextSnapshot, ContextError>> {
        self.inner.current_view(session)
    }

    fn trajectory_tail(&self, session: SessionId) -> Vec<SessionLogEntry> {
        self.inner.trajectory_tail(session)
    }

    fn trajectory_head(&self, session: SessionId) -> TrajectoryHead {
        self.inner.trajectory_head(session)
    }

    fn reset_trajectory(&self, session: SessionId) {
        self.inner.reset_trajectory(session);
    }

    fn resident_base(&self, session: SessionId) -> Result<Option<usize>, ContextError> {
        self.inner.resident_base(session)
    }

    fn trajectory_page(
        &self,
        session: SessionId,
        from: TrajectoryPosition,
        limit: usize,
    ) -> Result<TrajectoryPage, ContextError> {
        self.inner.trajectory_page(session, from, limit)
    }

    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        self.inner.write_checkpoint(manifest)
    }

    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        self.inner.read_checkpoint(id)
    }

    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        self.inner.list_checkpoints(session)
    }

    fn backend(&self) -> &dyn ContextBackend {
        self.inner.backend()
    }
}

/// Translates the command before it runs and records what it was given, so a
/// test can tell the typed text from the text the host ran.
struct TranslatingShell {
    ran: Arc<Mutex<Option<String>>>,
}

impl Tool for TranslatingShell {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "translates the command it is given"
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }
    fn resolved_input(&self, input: &Value) -> Option<Value> {
        let command = input.get("command")?.as_str()?;
        // An empty command has nothing to translate, the same way the real
        // tool leaves an unchanged command alone.
        if command.is_empty() {
            return None;
        }
        Some(json!({ "command": format!("{command} translated") }))
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let ran = self.ran.clone();
        Box::pin(async move {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .map(String::from);
            *ran.lock().expect("ran slot") = command;
            Ok(json!({ "success": true }))
        })
    }
}

/// A tool whose authorized entry refuses: the plain entry would run. This is
/// the shape the guarded bash tool has, so a test through it proves the
/// command reaches the entry a human decision authorized, not the guarded one.
struct PolicyDenied {
    calls: Arc<AtomicUsize>,
}

impl Tool for PolicyDenied {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "refuses at the authorized entry"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "success": true }))
        })
    }
    fn execute_authorized(
        &self,
        _ctx: ToolCtx,
        _input: Value,
    ) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move { Err(ToolError::Failed("denied by the permission gate".into())) })
    }
}

/// A project rule that refuses every call about to run.
struct RefuseAll;

impl Hook for RefuseAll {
    fn name(&self) -> &str {
        "refuse-all"
    }
    fn events(&self) -> &[HookEvent] {
        &[HookEvent::PreToolUse]
    }
    fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
        Ok(HookVerdict::Deny("project rule: not this command".into()))
    }
    fn source(&self) -> HookSource {
        HookSource::Project
    }
}

fn runner_with_shell(tool: EchoShell) -> (Runner, Arc<AtomicUsize>) {
    let calls = tool.calls.clone();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(tool));
    let runner = runner_with(Arc::new(FakeProvider::text("unused")), tools);
    (runner, calls)
}

fn echo_shell() -> EchoShell {
    EchoShell {
        calls: Arc::new(AtomicUsize::new(0)),
        failure: None,
    }
}

async fn tool_results(runner: &Runner, session: SessionId) -> Vec<Value> {
    let events = runner.store().replay(session).await.expect("replay");
    events
        .iter()
        .filter_map(|entry| match &entry.event {
            SessionEvent::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn test_call_and_result_recorded() {
    let (runner, calls) = runner_with_shell(echo_shell());
    let session = SessionId::new();
    let output = runner
        .run_direct_shell(session, "echo hi".into())
        .await
        .expect("the command answers");
    assert_eq!(output["stdout"], json!("ran echo hi"));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the command ran once");
    let events = runner.store().replay(session).await.expect("replay");
    let written: Vec<&SessionEvent> = events.iter().map(|entry| &entry.event).collect();
    assert!(
        matches!(
            written.as_slice(),
            [
                SessionEvent::ToolCall { .. },
                SessionEvent::ToolResult { .. }
            ]
        ),
        "the log holds the call and its result and no model turn: {written:?}"
    );
    let SessionEvent::ToolCall { call_id, .. } = written[0] else {
        unreachable!("the first record is the call");
    };
    let suffix = call_id
        .strip_prefix("shell-")
        .expect("a direct shell call is named as one");
    assert!(
        EventId::from_display_string(suffix).is_some(),
        "the id stays unique across restarts, not a counter that starts over: {call_id}"
    );
    assert!(
        is_user_shell_call_id(call_id),
        "a log reader tells the user's own command from a model call: {call_id}"
    );
}

#[tokio::test]
async fn test_cancel_stops_command() {
    let (started_tx, started_rx) = oneshot::channel();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HangingShell {
        started: Mutex::new(Some(started_tx)),
    }));
    let runner = runner_with(Arc::new(FakeProvider::text("unused")), tools);
    let session = SessionId::new();
    let (run, ()) = tokio::join!(runner.run_direct_shell(session, "sleep 60".into()), async {
        let _ = started_rx.await;
        runner.abort();
    });
    let output = run.expect("a cancelled command answers its result");
    assert_eq!(output["not_run"], json!(true));
    let results = tool_results(&runner, session).await;
    assert_eq!(
        results.len(),
        1,
        "the cancelled call keeps a result in the log"
    );
}

#[tokio::test]
async fn test_hook_denial_is_recorded() {
    let (runner, calls) = runner_with_shell(echo_shell());
    let rules = HookRegistry::new();
    rules.register(Arc::new(RefuseAll));
    let runner = runner.with_hooks(Arc::new(rules));
    let session = SessionId::new();
    let output = runner
        .run_direct_shell(session, "rm -rf build".into())
        .await
        .expect("a refused command answers its result");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a refused command does not run"
    );
    assert!(
        output["error"]
            .as_str()
            .is_some_and(|reason| reason.contains("blocked by hook")),
        "the refusal names the rule: {output}"
    );
    assert_eq!(tool_results(&runner, session).await.len(), 1);
}

#[tokio::test]
async fn test_failure_becomes_result() {
    let (runner, _) = runner_with_shell(EchoShell {
        calls: Arc::new(AtomicUsize::new(0)),
        failure: Some("sandbox refused the process".into()),
    });
    let session = SessionId::new();
    let output = runner
        .run_direct_shell(session, "make".into())
        .await
        .expect("a failed command still answers its result");
    assert!(
        output["error"]
            .as_str()
            .is_some_and(|reason| reason.contains("sandbox refused")),
        "the tool failure travels as the result: {output}"
    );
    assert_eq!(
        tool_results(&runner, session).await.len(),
        1,
        "the failure is written to the log, so no ToolCall is left without a result"
    );
}

/// A cancel issued while the command is still being recorded stops it: the
/// cancellation is already in force when the command would start, so the call
/// answers as not run instead of running to completion.
#[tokio::test]
async fn test_cancel_before_start_stops() {
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let log: Arc<dyn SessionLog> = Arc::new(SlowFirstAppend {
        inner: Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        reached: Mutex::new(Some(reached_tx)),
        resume: Mutex::new(Some(release_rx)),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HangingShell {
        started: Mutex::new(None),
    }));
    let runner = Arc::new(Runner::new(
        log,
        Arc::new(FakeProvider::text("unused")),
        tools,
        RunnerConfig::default(),
    ));
    let session = SessionId::new();
    let shell = tokio::spawn({
        let runner = Arc::clone(&runner);
        async move { runner.run_direct_shell(session, "sleep 60".into()).await }
    });
    reached_rx.await.expect("the call append starts");
    runner.abort();
    release_tx.send(()).expect("the append resumes");
    let run = timeout(Duration::from_secs(2), shell)
        .await
        .expect("the cancelled command answers instead of running")
        .expect("the task does not panic");
    assert_eq!(
        run.expect("the cancelled command answers its result")["not_run"],
        json!(true)
    );
}

#[tokio::test]
async fn test_typed_command_recorded() {
    let ran = Arc::new(Mutex::new(None));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(TranslatingShell {
        ran: Arc::clone(&ran),
    }));
    let runner = runner_with(Arc::new(FakeProvider::text("unused")), tools);
    let session = SessionId::new();
    runner
        .run_direct_shell(session, "echo hi".into())
        .await
        .expect("the command answers");
    assert_eq!(
        ran.lock().expect("ran slot").as_deref(),
        Some("echo hi translated"),
        "the host runs the resolved text"
    );
    let events = runner.store().replay(session).await.expect("replay");
    let SessionEvent::ToolCall { input, .. } = &events[0].event else {
        panic!("the first record is the call: {:?}", events[0].event);
    };
    assert_eq!(
        input["command"],
        json!("echo hi"),
        "the log keeps the command as the user typed it"
    );
}

/// Neither an empty command nor one holding only whitespace carries text to
/// run. The terminal refuses both before submitting, so this guard covers
/// every other client: the refusal writes nothing to the log.
#[tokio::test]
async fn test_empty_command_refused() {
    let (runner, calls) = runner_with_shell(echo_shell());
    let session = SessionId::new();
    for command in ["", "   "] {
        let error = runner
            .run_direct_shell(session, command.into())
            .await
            .expect_err("a command with no text is refused");
        assert!(
            error.to_string().contains("empty"),
            "the refusal names the blank text for {command:?}: {error}"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the command did not run");
    let events = runner.store().replay(session).await.expect("replay");
    assert!(
        events.is_empty(),
        "a refused command writes nothing: {events:?}"
    );
}

#[tokio::test]
async fn test_denied_entry_refused() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(PolicyDenied {
        calls: Arc::clone(&calls),
    }));
    let runner = runner_with(Arc::new(FakeProvider::text("unused")), tools);
    let session = SessionId::new();
    let output = runner
        .run_direct_shell(session, "rm -rf build".into())
        .await
        .expect("a refused command answers its result");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the command reaches the authorized entry, which refuses it"
    );
    assert!(
        output["error"]
            .as_str()
            .is_some_and(|reason| reason.contains("denied by the permission gate")),
        "the refusal is the result: {output}"
    );
    assert_eq!(tool_results(&runner, session).await.len(), 1);
}

/// A model-issued tool call that starts, reports, and never returns: only the
/// abort can end the turn it runs in.
struct SlowTool {
    started: Mutex<Option<oneshot::Sender<()>>>,
}

impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "stands in for a model tool call that does not finish"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let started = self.started.lock().expect("started slot").take();
        Box::pin(async move {
            if let Some(report) = started {
                let _ = report.send(());
            }
            pending::<()>().await;
            Ok(Value::Null)
        })
    }
}

fn tool_call_response(tool: &str) -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "call-1".into(),
            name: tool.into(),
            input: json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

fn final_response() -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

/// A command submitted while a model turn is still running is refused before
/// it writes anything, and the turn keeps the cancel slot: the abort that
/// follows still stops it. A command that took the slot would leave the turn
/// running with no way to stop it.
#[tokio::test]
async fn test_command_refused_during_turn() {
    let (started_tx, started_rx) = oneshot::channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoShell {
        calls: Arc::clone(&calls),
        failure: None,
    }));
    tools.register(Arc::new(SlowTool {
        started: Mutex::new(Some(started_tx)),
    }));
    let provider = FakeProvider::new(vec![tool_call_response("slow"), final_response()]);
    let runner = Arc::new(runner_with(Arc::new(provider), tools));
    let session = SessionId::new();
    let run = tokio::spawn({
        let runner = Arc::clone(&runner);
        async move { runner.run(session, "work".into()).await }
    });
    started_rx.await.expect("the model call reached the tool");
    let error = runner
        .run_direct_shell(session, "echo hi".into())
        .await
        .expect_err("a command cannot start beside a running turn");
    assert!(
        error.to_string().contains("in progress"),
        "the refusal names the running turn: {error}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the command did not run");
    runner.abort();
    let result = timeout(Duration::from_secs(2), run)
        .await
        .expect("the abort still reaches the running turn")
        .expect("the run task does not panic")
        .expect("the aborted turn answers");
    assert!(
        matches!(result.outcome, RunOutcome::Interrupted(_)),
        "the abort reached the turn: {:?}",
        result.outcome
    );
    let events = runner.store().replay(session).await.expect("replay");
    assert!(
        !events.iter().any(|entry| matches!(
            &entry.event,
            SessionEvent::ToolCall { call_id, .. } if is_user_shell_call_id(call_id)
        )),
        "a refused command writes no call: {events:?}"
    );
}

#[tokio::test]
async fn test_paused_run_refuses() {
    let (runner, calls) = runner_with_shell(echo_shell());
    runner.mark_paused();
    let session = SessionId::new();
    let error = runner
        .run_direct_shell(session, "echo hi".into())
        .await
        .expect_err("a paused run leaves no room for a command");
    assert!(
        error.to_string().contains("approval decision"),
        "the refusal names the wait: {error}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the command did not run");
    let events = runner.store().replay(session).await.expect("replay");
    assert!(
        events.is_empty(),
        "a refused command writes nothing: {events:?}"
    );
}

#[tokio::test]
async fn test_without_bash_tool_fails() {
    let runner = runner_with(Arc::new(FakeProvider::text("unused")), ToolRegistry::new());
    let error = runner
        .run_direct_shell(SessionId::new(), "echo hi".into())
        .await
        .expect_err("no shell tool answers nothing");
    assert!(
        error.to_string().contains("no bash tool"),
        "the error names the missing tool: {error}"
    );
}
