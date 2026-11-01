//! A session paused on an approval, reopened from its durable log by a
//! rebuilt runner. The first runner parks on a file edit and ends; the second
//! opens the same session directory and applies the decision through the
//! public resume. The check the first runner already answered must keep that
//! answer instead of running against source the edit may not have written.
//! The in-crate approval tests drive the decision entry point directly; this
//! drives the entry point a later process calls.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};

use houyicoder_api::provider::{ModelProvider, stream_from_response};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{ApprovalDecision, RunOutcome, Runner, ToolRegistry};
use houyicoder_memory::LocalFileBackend;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_session::SessionStore;
use serde_json::{Value, json};

/// Answers canned responses in order and repeats the last one, so the model
/// call that continues a resume reaches an answer instead of running out.
struct StubProvider {
    responses: Mutex<Vec<CompletionResponse>>,
}

impl StubProvider {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        assert!(!responses.is_empty(), "a stub needs a response");
        Self {
            responses: Mutex::new(responses),
        }
    }

    fn next(&self) -> CompletionResponse {
        let mut responses = self.responses.lock().expect("response list");
        if responses.len() > 1 {
            responses.remove(0)
        } else {
            responses[0].clone()
        }
    }
}

impl ModelProvider for StubProvider {
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

/// An edit that asks before it runs and counts its attempts, so a decision
/// that ran it twice or never ran it is visible.
struct ParkingEdit {
    attempts: Arc<Mutex<u32>>,
}

impl ParkingEdit {
    fn new() -> Self {
        Self {
            attempts: Arc::new(Mutex::new(0)),
        }
    }

    fn attempts(&self) -> u32 {
        *self.attempts.lock().expect("attempt count")
    }
}

impl Tool for ParkingEdit {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "writes a file after the user approves it"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        *self.attempts.lock().expect("attempt count") += 1;
        Box::pin(async { Ok(json!({"success": true})) })
    }

    fn requires_approval(&self) -> bool {
        true
    }
}

/// The shell check the edit's response sends. It records every command that
/// reached it, so a check that ran while its edit was undecided is visible.
struct ShellCheck {
    commands: Arc<Mutex<Vec<String>>>,
}

impl ShellCheck {
    fn new() -> Self {
        Self {
            commands: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().expect("command list").clone()
    }
}

impl Tool for ShellCheck {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "runs a shell command"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let command = input["command"].as_str().unwrap_or_default();
        self.commands
            .lock()
            .expect("command list")
            .push(command.to_string());
        Box::pin(async { Ok(json!({"success": true})) })
    }
}

/// One response that edits a file and then runs the check that reads it.
fn edit_then_check() -> CompletionResponse {
    CompletionResponse {
        output: vec![
            OutputItem::ToolCall {
                id: "call_edit".into(),
                name: "edit".into(),
                input: json!({"path": "src/lib.rs"}),
            },
            OutputItem::ToolCall {
                id: "call_check".into(),
                name: "bash".into(),
                input: json!({"command": "cargo test"}),
            },
        ],
        usage: Usage::default(),
        model: "test".into(),
    }
}

/// The answer the resumed model call gives so the run ends.
fn done() -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

fn store_on(root: &Path) -> Arc<SessionStore> {
    Arc::new(SessionStore::new(Box::new(LocalFileBackend::new(
        root.to_path_buf(),
    ))))
}

fn runner_on(
    store: Arc<SessionStore>,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
) -> Runner {
    Runner::new(store, provider, tools, RunnerConfig::default())
}

/// A session directory removed when the test ends, whichever path it took.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        let root = env::temp_dir().join(format!("houyi-resume-{tag}-{}", process::id()));
        fs::remove_dir_all(&root).ok();
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

/// The calls the model emitted, in append order.
fn calls(events: &[SessionLogEntry]) -> Vec<String> {
    events
        .iter()
        .filter_map(|entry| match &entry.event {
            SessionEvent::ToolCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

/// Every answer on the log, keyed by the call it answers. One answer per call
/// is part of the contract: a resumed run that recorded a second answer would
/// leave the model reading two verdicts for one call.
fn recorded(events: &[SessionLogEntry]) -> HashMap<String, Value> {
    let mut results = HashMap::new();
    for entry in events {
        if let SessionEvent::ToolResult {
            call_id, output, ..
        } = &entry.event
        {
            assert!(
                results.insert(call_id.clone(), output.clone()).is_none(),
                "call {call_id} has more than one answer"
            );
        }
    }
    results
}

/// Each emitted call's answer, read from the durable log.
fn answers(events: &[SessionLogEntry]) -> HashMap<String, Value> {
    let calls = calls(events);
    let results = recorded(events);
    assert_eq!(calls.len(), 2, "the response emitted two calls");
    for call_id in calls {
        assert!(
            results.contains_key(&call_id),
            "call {call_id} has no answer in the log"
        );
    }
    results
}

/// Run the first runner to its pause on the edit and drop it. The returned
/// answers are what the log holds before the second runner opens it: the edit
/// unanswered, and the check already settled as skipped.
async fn parked_on_disk(tag: &str) -> (TempRoot, SessionId, HashMap<String, Value>) {
    let root = TempRoot::new(tag);
    let edit = Arc::new(ParkingEdit::new());
    let shell = Arc::new(ShellCheck::new());
    let mut tools = ToolRegistry::new();
    tools.register(edit.clone());
    tools.register(shell.clone());
    let session = SessionId::new();
    {
        let runner = runner_on(
            store_on(root.path()),
            Arc::new(StubProvider::new(vec![edit_then_check()])),
            tools,
        );
        let paused = runner
            .run(session, "fix the bug".into())
            .await
            .expect("run");
        match paused.outcome {
            RunOutcome::Interruption(requests) => {
                assert_eq!(requests.len(), 1, "only the edit awaits a verdict");
                assert_eq!(requests[0].call_id, "call_edit");
            }
            other => panic!("expected the edit to await approval, got {other:?}"),
        }
        assert_eq!(edit.attempts(), 0, "the edit did not run before approval");
        assert!(
            shell.commands().is_empty(),
            "the check did not run while its edit was undecided"
        );
    }
    let events = store_on(root.path()).replay(session).await.expect("replay");
    assert_eq!(calls(&events).len(), 2, "the response emitted two calls");
    let before = recorded(&events);
    assert!(
        !before.contains_key("call_edit"),
        "the parked edit has no answer yet"
    );
    let check = before
        .get("call_check")
        .expect("the check is answered before the restart");
    assert_eq!(check["not_run"], true, "the recorded answer is not a run");
    let reason = check["error"]
        .as_str()
        .expect("the recorded answer explains why it did not run");
    assert!(
        reason.starts_with("skipped:"),
        "the recorded answer names the unfinished edit: {reason}"
    );
    (root, session, before)
}

/// A second runner over the same session directory, with fresh tool
/// instances so the first runner's records cannot answer for it.
fn reopened(root: &Path) -> (Runner, Arc<ParkingEdit>, Arc<ShellCheck>) {
    let edit = Arc::new(ParkingEdit::new());
    let shell = Arc::new(ShellCheck::new());
    let mut tools = ToolRegistry::new();
    tools.register(edit.clone());
    tools.register(shell.clone());
    let runner = runner_on(
        store_on(root),
        Arc::new(StubProvider::new(vec![done()])),
        tools,
    );
    (runner, edit, shell)
}

/// The second runner approves the edit. The edit runs there, and the check
/// keeps the answer recorded before the restart instead of running against
/// source the first runner never wrote.
#[tokio::test]
async fn test_resume_runs_approved_edit() {
    let (root, session, before) = parked_on_disk("approve").await;
    let (runner, edit, shell) = reopened(root.path());
    // The composition root seeds a reopened session from its log before it
    // resumes, so this mirrors it.
    runner.seed_session_from_log(session);

    let result = runner
        .resume(session, &[ApprovalDecision::approve("call_edit")])
        .await
        .expect("resume");

    assert!(
        matches!(result.outcome, RunOutcome::FinalOutput(_)),
        "the resumed turn reaches its answer, got {:?}",
        result.outcome
    );
    assert_eq!(edit.attempts(), 1, "the approved edit runs in this runner");
    assert!(shell.commands().is_empty(), "the check did not run");
    let after = answers(&store_on(root.path()).replay(session).await.expect("replay"));
    assert_eq!(
        after.get("call_edit"),
        Some(&json!({"success": true})),
        "the value the edit returned is the recorded answer"
    );
    assert_eq!(
        after.get("call_check"),
        before.get("call_check"),
        "the recorded skip survives the resume"
    );
}

/// The second runner declines the edit instead. The edit does not run, and the
/// check keeps the answer recorded before the restart rather than being
/// offered again as if the edit might still write.
#[tokio::test]
async fn test_resume_skips_rejected_edit() {
    let (root, session, before) = parked_on_disk("reject").await;
    let (runner, edit, shell) = reopened(root.path());
    // Seeded the same way, so this case drives the entry point a later
    // process uses.
    runner.seed_session_from_log(session);

    let result = runner
        .resume(session, &[ApprovalDecision::reject("call_edit")])
        .await
        .expect("resume");

    assert!(
        matches!(result.outcome, RunOutcome::FinalOutput(_)),
        "the resumed turn reaches its answer, got {:?}",
        result.outcome
    );
    assert_eq!(edit.attempts(), 0, "the declined edit did not run");
    assert!(shell.commands().is_empty(), "the check did not run");
    let after = answers(&store_on(root.path()).replay(session).await.expect("replay"));
    assert_eq!(
        after.get("call_edit"),
        Some(&json!({
            "error": "rejected by user",
            "not_run": true,
            "refused": {"by": "user", "verdict": "reject"},
        })),
        "the refusal names who declined the call"
    );
    assert_eq!(
        after.get("call_check"),
        before.get("call_check"),
        "the recorded skip survives the resume"
    );
}
