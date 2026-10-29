//! Tests for this module.

use super::*;
use crate::agent::tests::runner_with;
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
