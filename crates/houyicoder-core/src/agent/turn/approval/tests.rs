//! Tests for this module.

use super::*;
use crate::agent::append::new_event;
use crate::agent::tests::runner_with;
use crate::agent::turn::fallback::SKIPPED_AFTER_EDIT;
use crate::agent::{ApprovalDecision, RunOutcome, ToolRegistry};
use crate::provider::test_support::FakeProvider;
use houyicoder_api::skill::{SkillFamily, SkillProvenance, SkillSource};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::process;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

fn user_subject(skill: &str) -> GrantSubject {
    SkillSource::new(SkillFamily::Houyi, SkillProvenance::UserHome).grant_subject(skill)
}

#[test]
fn test_entitlement_grants_services() {
    let dir = env::temp_dir().join(format!("houyi-entitlement-grant-{}", process::id()));
    let _ = fs::remove_dir_all(&dir).is_ok();
    fs::create_dir_all(&dir).expect("mkdir grant test");
    let path = dir.join("skill-grants.json");
    let store = SkillGrantStore::with_path(path);
    let input = serde_json::json!({
        "skill": "ego-browser",
        "grant_subject": user_subject("ego-browser").to_json(),
        "services": ["com.apple.trustd", "com.houyi.test.entitlement"],
    });
    let result = apply_entitlement(&input, Some(&store));
    assert_eq!(
        result["granted"],
        serde_json::json!(["com.houyi.test.entitlement"])
    );
    assert_eq!(
        result["message"],
        "Authorized 1 service(s) for ego-browser."
    );
    assert!(
        store
            .grant_for(&user_subject("ego-browser"))
            .contains(&"com.houyi.test.entitlement".to_string())
    );
    let _ = fs::remove_dir_all(&dir).is_ok();
}

#[test]
fn test_entitlement_persist_failure() {
    let dir = env::temp_dir().join(format!("houyi-entitlement-error-{}", process::id()));
    let _cleanup = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir persistence test");
    let store = SkillGrantStore::with_path(dir.clone());
    let input = serde_json::json!({
        "skill": "ego-browser",
        "grant_subject": user_subject("ego-browser").to_json(),
        "services": ["com.houyi.test.entitlement"],
    });
    let result = apply_entitlement(&input, Some(&store));
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|e| e.contains("persistence failed"))
    );
    assert!(result["granted"].as_array().is_some_and(Vec::is_empty));
    assert!(store.grant_for(&user_subject("ego-browser")).is_empty());
    let _cleanup = fs::remove_dir_all(&dir);
}

#[test]
fn test_entitlement_no_store() {
    let input = serde_json::json!({
        "skill": "x",
        "grant_subject": user_subject("x").to_json(),
        "services": ["a.b.c"],
    });
    let result = apply_entitlement(&input, None);
    assert_eq!(result["error"], "grant store not wired");
}

#[test]
fn test_entitlement_empty_skill_refused() {
    let dir = env::temp_dir().join(format!("houyi-entitlement-empty-{}", process::id()));
    let _ = fs::remove_dir_all(&dir).is_ok();
    fs::create_dir_all(&dir).expect("mkdir empty-skill test");
    let path = dir.join("skill-grants.json");
    let store = SkillGrantStore::with_path(path);
    let input = serde_json::json!({ "skill": "", "services": ["a.b.c"] });
    let result = apply_entitlement(&input, Some(&store));
    assert_eq!(result["error"], "no skill named");
    assert!(
        store.grant_for(&user_subject("unused")).is_empty(),
        "empty-name grant must not be written"
    );
    let _ = fs::remove_dir_all(&dir).is_ok();
}

/// A tool whose host runs a translated command, recording what it ran.
struct TranslatingTool {
    ran: Arc<Mutex<Vec<Value>>>,
}

impl Tool for TranslatingTool {
    fn name(&self) -> &str {
        "translating"
    }
    fn description(&self) -> &str {
        ""
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {"command": {"type": "string"}}})
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        self.ran.lock().expect("ran lock").push(input);
        Box::pin(async { Ok(Value::Null) })
    }
    fn requires_approval(&self) -> bool {
        true
    }
    fn resolved_input(&self, input: &Value) -> Option<Value> {
        let command = input.get("command")?.as_str()?;
        let translated = command.replace(">nul", ">/dev/null");
        if translated == command {
            return None;
        }
        Some(json!({"command": translated}))
    }
}

/// An approved call resumes from the log, where the model's own text is
/// recorded, so the resume path resolves the translation again: the card
/// offers the command the host runs and that command is what executes. A
/// resume that ran the log's text would execute a command the gate never
/// judged.
#[tokio::test]
async fn test_resume_runs_translation() {
    let call = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "translating".into(),
            input: json!({"command": "run >nul"}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let answer = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let ran = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(TranslatingTool {
        ran: Arc::clone(&ran),
    }));
    let runner = runner_with(Arc::new(FakeProvider::new(vec![call, answer])), tools);
    let session = SessionId::new();
    let paused = runner.run(session, "go".into()).await.expect("run ok");
    let approvals = match paused.outcome {
        RunOutcome::Interruption(approvals) => approvals,
        other => panic!("expected the call to await approval, got {other:?}"),
    };
    assert_eq!(
        approvals[0].input["command"], "run >/dev/null",
        "the card offers the command the host runs"
    );
    let resumed = runner
        .resume(session, &[ApprovalDecision::approve("c1")])
        .await
        .expect("resume ok");
    assert!(
        matches!(resumed.outcome, RunOutcome::FinalOutput(_)),
        "the approved call runs and the turn answers: {:?}",
        resumed.outcome
    );
    let executed = {
        let ran = ran.lock().expect("ran lock");
        assert_eq!(ran.len(), 1, "the approved call ran once");
        ran[0]["command"].clone()
    };
    assert_eq!(
        executed, "run >/dev/null",
        "the approved call runs the translated command"
    );
    let events = runner.store().replay(session).await.expect("replay");
    let logged: Vec<&Value> = events
        .iter()
        .filter_map(|entry| match &entry.event {
            SessionEvent::ToolCall { input, .. } => Some(input),
            _ => None,
        })
        .collect();
    assert_eq!(
        logged,
        vec![&json!({"command": "run >nul"})],
        "the transcript keeps the model's own call"
    );
}

/// An edit that fails when it runs, counting its attempts.
struct FailingEdit {
    ran: Arc<Mutex<u32>>,
}

impl FailingEdit {
    fn attempts(&self) -> u32 {
        *self.ran.lock().expect("ran lock")
    }
}

impl Tool for FailingEdit {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        ""
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        *self.ran.lock().expect("ran lock") += 1;
        Box::pin(async { Err(ToolError::InvalidInput("old_string not found".into())) })
    }
}

/// An edit that reports success without touching the disk.
struct SucceedingEdit;

impl Tool for SucceedingEdit {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        ""
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(json!({"success": true})) })
    }
}

/// A shell that records the commands it runs.
struct RecordingShell {
    commands: Arc<Mutex<Vec<String>>>,
}

impl RecordingShell {
    fn ran(&self) -> Vec<String> {
        self.commands.lock().expect("commands lock").clone()
    }
}

impl Tool for RecordingShell {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        ""
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {"command": {"type": "string"}}})
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.commands.lock().expect("commands lock").push(command);
        Box::pin(async { Ok(json!({"success": true})) })
    }
}

/// A batch parked before an edit ran: the calls are in the log with no
/// answers, the way a crash or an older build leaves them.
fn parked_batch() -> (Runner, SessionId, Arc<FailingEdit>, Arc<RecordingShell>) {
    let edit = Arc::new(FailingEdit {
        ran: Arc::new(Mutex::new(0)),
    });
    let shell = Arc::new(RecordingShell {
        commands: Arc::new(Mutex::new(Vec::new())),
    });
    let mut tools = ToolRegistry::new();
    tools.register(edit.clone());
    tools.register(shell.clone());
    let runner = runner_with(Arc::new(FakeProvider::text("done")), tools);
    (runner, SessionId::new(), edit, shell)
}

async fn seed_parked(runner: &Runner, session: SessionId) {
    runner
        .append_tool_call(session, "call_edit", "edit", json!({}))
        .await
        .expect("seed the edit");
    runner
        .append_tool_call(
            session,
            "call_bash",
            "bash",
            json!({"command": "cargo test"}),
        )
        .await
        .expect("seed the check");
}

async fn logged_skip(runner: &Runner, session: SessionId) -> bool {
    runner
        .store()
        .replay(session)
        .await
        .expect("replay")
        .iter()
        .any(|entry| {
            matches!(&entry.event, SessionEvent::ToolResult { call_id, output, .. }
            if call_id == "call_bash"
                && output["error"] == SKIPPED_AFTER_EDIT
                && output["not_run"] == true)
        })
}

/// Both calls of a parked batch are approved together. The edit fails while
/// running, so the check is answered as skipped instead of running against
/// source the edit never wrote.
#[tokio::test]
async fn test_approved_check_skipped() {
    let (runner, session, edit, shell) = parked_batch();
    seed_parked(&runner, session).await;

    let remaining = runner
        .apply_decisions(
            session,
            &[
                ApprovalDecision::approve("call_edit"),
                ApprovalDecision::approve("call_bash"),
            ],
            &CancellationToken::new(),
        )
        .await
        .expect("decisions");

    assert!(remaining.is_empty(), "both calls are answered");
    assert_eq!(edit.attempts(), 1, "the edit was attempted");
    assert!(shell.ran().is_empty(), "the check did not run");
    assert!(logged_skip(&runner, session).await, "the check is answered");
}

/// Only the edit is decided. The check it follows can never run, so it is
/// answered now instead of being offered on a later card.
#[tokio::test]
async fn test_undecided_check_skipped() {
    let (runner, session, edit, shell) = parked_batch();
    seed_parked(&runner, session).await;

    let remaining = runner
        .apply_decisions(
            session,
            &[ApprovalDecision::approve("call_edit")],
            &CancellationToken::new(),
        )
        .await
        .expect("decisions");

    assert!(remaining.is_empty(), "no card is left for the check");
    assert_eq!(edit.attempts(), 1, "the edit was attempted");
    assert!(shell.ran().is_empty(), "the check did not run");
    assert!(logged_skip(&runner, session).await, "the check is answered");
}

/// Both calls of a parked batch are approved and the edit succeeds, so the
/// check runs in the same resume: the two calls the model sent in one response
/// are released without another model step.
#[tokio::test]
async fn test_approved_edit_releases_check() {
    let shell = Arc::new(RecordingShell {
        commands: Arc::new(Mutex::new(Vec::new())),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SucceedingEdit));
    tools.register(shell.clone());
    let runner = runner_with(Arc::new(FakeProvider::text("done")), tools);
    let session = SessionId::new();
    seed_parked(&runner, session).await;

    let remaining = runner
        .apply_decisions(
            session,
            &[
                ApprovalDecision::approve("call_edit"),
                ApprovalDecision::approve("call_bash"),
            ],
            &CancellationToken::new(),
        )
        .await
        .expect("decisions");

    assert!(remaining.is_empty(), "both calls are answered");
    assert_eq!(
        shell.ran(),
        vec!["cargo test".to_string()],
        "the approved check runs on its own edit's success"
    );
}

/// Only the check is decided. It must not run while the edit it follows is
/// still undecided: that edit may yet write the source the check reads.
#[tokio::test]
async fn test_approved_check_waits() {
    let (runner, session, edit, shell) = parked_batch();
    seed_parked(&runner, session).await;

    let remaining = runner
        .apply_decisions(
            session,
            &[ApprovalDecision::approve("call_bash")],
            &CancellationToken::new(),
        )
        .await
        .expect("decisions");

    assert_eq!(edit.attempts(), 0, "the undecided edit did not run");
    assert!(
        shell.ran().is_empty(),
        "an approved check waits for its edit"
    );
    assert_eq!(remaining.len(), 2, "both calls await a later verdict");
    assert!(
        !logged_skip(&runner, session).await,
        "nothing is answered before the edit is settled"
    );
}

/// The log already records that the edit before the check failed, as a
/// process that stopped mid-response leaves it. The resume settles the check
/// from that record and does not run the edit again.
#[tokio::test]
async fn test_recorded_failure_skips_check() {
    let (runner, session, edit, shell) = parked_batch();
    runner
        .append_tool_call(session, "call_edit", "edit", json!({}))
        .await
        .expect("seed the edit");
    runner
        .append_tool_result(
            session,
            "call_edit".into(),
            "edit",
            json!({"error": "old_string not found"}),
            0,
        )
        .await
        .expect("seed the edit's failure");
    runner
        .append_tool_call(
            session,
            "call_bash",
            "bash",
            json!({"command": "cargo test"}),
        )
        .await
        .expect("seed the check");

    let remaining = runner
        .apply_decisions(
            session,
            &[ApprovalDecision::approve("call_bash")],
            &CancellationToken::new(),
        )
        .await
        .expect("decisions");

    assert!(remaining.is_empty(), "the check is answered");
    assert_eq!(edit.attempts(), 0, "the settled edit is not run again");
    assert!(shell.ran().is_empty(), "the check did not run");
    assert!(logged_skip(&runner, session).await, "the check is answered");
}

/// An edit the run was interrupted under wrote nothing either. On the resume
/// the model does not know that, so a check the human approves would run
/// against source the edit never changed; the log says otherwise, and the
/// check is answered as skipped.
#[tokio::test]
async fn test_interrupted_edit_skips_check() {
    let (runner, session, edit, shell) = parked_batch();
    for event in [
        SessionEvent::ToolCall {
            call_id: "call_edit".into(),
            tool: "edit".into(),
            input: json!({}),
        },
        SessionEvent::ToolResult {
            call_id: "call_edit".into(),
            output: json!({"error": "interrupted by user", "not_run": true}),
            duration_ms: 0,
        },
        SessionEvent::ToolCall {
            call_id: "call_bash".into(),
            tool: "bash".into(),
            input: json!({"command": "cargo test"}),
        },
    ] {
        runner
            .store()
            .append(new_event(session, event))
            .await
            .expect("seed the log");
    }

    let remaining = runner
        .apply_decisions(
            session,
            &[ApprovalDecision::approve("call_bash")],
            &CancellationToken::new(),
        )
        .await
        .expect("decisions");

    assert!(remaining.is_empty(), "the check is answered");
    assert_eq!(edit.attempts(), 0, "the interrupted edit is not run again");
    assert!(shell.ran().is_empty(), "the check did not run");
    assert!(logged_skip(&runner, session).await, "the check is answered");
}

/// A declined edit wrote nothing, so the check behind it is answered as
/// skipped rather than offered on a later card.
#[tokio::test]
async fn test_rejected_edit_skips_check() {
    let (runner, session, edit, shell) = parked_batch();
    seed_parked(&runner, session).await;

    let remaining = runner
        .apply_decisions(
            session,
            &[
                ApprovalDecision::reject("call_edit"),
                ApprovalDecision::approve("call_bash"),
            ],
            &CancellationToken::new(),
        )
        .await
        .expect("decisions");

    assert!(remaining.is_empty(), "both calls are answered");
    assert_eq!(edit.attempts(), 0, "the declined edit did not run");
    assert!(shell.ran().is_empty(), "the check did not run");
    assert!(logged_skip(&runner, session).await, "the check is answered");
}

/// The check rule is settled per response. An edit that failed in an earlier
/// response wrote a source the model has since seen; a check the model sends
/// in a later response is testing that state, so it runs. Reading the earlier
/// failure as an anchor would answer a command the model meant to run.
#[tokio::test]
async fn test_later_check_still_runs() {
    let (runner, session, _edit, shell) = parked_batch();
    for event in [
        SessionEvent::AssistantMessage {
            text: String::new(),
            thinking: None,
        },
        SessionEvent::ToolCall {
            call_id: "call_edit".into(),
            tool: "edit".into(),
            input: json!({}),
        },
        SessionEvent::ToolResult {
            call_id: "call_edit".into(),
            output: json!({"error": "old_string not found"}),
            duration_ms: 0,
        },
        SessionEvent::AssistantMessage {
            text: String::new(),
            thinking: None,
        },
        SessionEvent::ToolCall {
            call_id: "call_bash".into(),
            tool: "bash".into(),
            input: json!({"command": "cargo test"}),
        },
    ] {
        runner
            .store()
            .append(new_event(session, event))
            .await
            .expect("seed the log");
    }

    let remaining = runner
        .apply_decisions(
            session,
            &[ApprovalDecision::approve("call_bash")],
            &CancellationToken::new(),
        )
        .await
        .expect("decisions");

    assert!(remaining.is_empty(), "the check is answered");
    assert_eq!(
        shell.ran(),
        vec!["cargo test".to_string()],
        "a later response's check does not depend on an earlier response's edit"
    );
    assert!(
        !logged_skip(&runner, session).await,
        "the check is not answered as skipped"
    );
}

/// The host no longer registers the edit the batch parked. None of it ran, so
/// the check behind it is answered as skipped instead of running.
#[tokio::test]
async fn test_unknown_edit_skips_check() {
    let (runner, session, _edit, shell) = parked_batch();
    runner
        .append_tool_call(session, "call_edit", "multiedit", json!({}))
        .await
        .expect("seed the edit");
    runner
        .append_tool_call(
            session,
            "call_bash",
            "bash",
            json!({"command": "cargo test"}),
        )
        .await
        .expect("seed the check");

    let remaining = runner
        .apply_decisions(
            session,
            &[
                ApprovalDecision::approve("call_edit"),
                ApprovalDecision::approve("call_bash"),
            ],
            &CancellationToken::new(),
        )
        .await
        .expect("decisions");

    assert!(remaining.is_empty(), "both calls are answered");
    assert!(shell.ran().is_empty(), "the check did not run");
    assert!(logged_skip(&runner, session).await, "the check is answered");
}
