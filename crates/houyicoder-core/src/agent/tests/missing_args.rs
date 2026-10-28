//! A tool call whose arguments are missing or still streaming when the reply
//! was cut. Neither can run, so neither is worth a human interruption: the
//! call is answered with the tool's own schema error and the model corrects
//! itself.

use houyicoder_async::PFut;
use serde_json::{Value, json};

use super::length::ScriptRawProvider;
use super::*;

/// A tool that takes one required argument and refuses a call without it, the
/// shape every built-in argument-taking tool has.
struct ArgTool;

impl Tool for ArgTool {
    fn name(&self) -> &str {
        "arg"
    }
    fn description(&self) -> &str {
        "a tool that needs one argument"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "required": ["cmd"], "properties": {"cmd": {"type": "string"}}})
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            let cmd = input
                .get("cmd")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidInput("arg: cmd (string) required".into()))?;
            Ok(json!({"ran": cmd}))
        })
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

/// The same argument contract without an approval gate, so a test can watch a
/// well-formed call reach the log with no human in the way.
struct PlainTool;

impl Tool for PlainTool {
    fn name(&self) -> &str {
        "plain"
    }
    fn description(&self) -> &str {
        "a tool that needs one argument and runs without asking"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "required": ["cmd"], "properties": {"cmd": {"type": "string"}}})
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            let cmd = input
                .get("cmd")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidInput("plain: cmd (string) required".into()))?;
            Ok(json!({"ran": cmd}))
        })
    }
}

fn arg_registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ArgTool));
    tools.register(Arc::new(PlainTool));
    tools
}

fn response(output: Vec<OutputItem>) -> CompletionResponse {
    CompletionResponse {
        output,
        usage: Usage::default(),
        model: "test".into(),
    }
}

/// A call the model emitted without its required argument is answered with the
/// schema message the model can act on, before the gate is consulted. Asking a
/// human first would spend an interruption on a call that cannot run.
#[tokio::test]
async fn test_missing_argument_skips_ask() {
    let p = Arc::new(FakeProvider::new(vec![
        response(vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "arg".into(),
            input: json!({}),
        }]),
        response(vec![OutputItem::Text {
            text: "corrected".into(),
        }]),
    ]));
    let runner = runner_with(p, arg_registry());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    match result.outcome {
        RunOutcome::FinalOutput(text) => assert_eq!(text, "corrected"),
        other => panic!("a call that cannot run must not pause the run, got {other:?}"),
    }
    let replay = runner.store().replay(session).await.expect("replay");
    let refusal = replay.iter().find_map(|entry| match &entry.event {
        SessionEvent::ToolResult {
            call_id, output, ..
        } if call_id == "c1" => Some(output.clone()),
        _ => None,
    });
    let refusal = refusal.expect("the tool must answer the call it refused");
    assert!(
        refusal.to_string().contains("cmd (string) required"),
        "the model must see why the call failed: {refusal}"
    );
}

/// A present but unusable value is the same class as an absent one: an empty
/// command string cannot run, so it must not reach the gate either.
#[tokio::test]
async fn test_empty_argument_skips_ask() {
    let p = Arc::new(FakeProvider::new(vec![
        response(vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "arg".into(),
            input: json!({"cmd": ""}),
        }]),
        response(vec![OutputItem::Text {
            text: "corrected".into(),
        }]),
    ]));
    let runner = runner_with(p, arg_registry());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    match result.outcome {
        RunOutcome::FinalOutput(text) => assert_eq!(text, "corrected"),
        other => panic!("an empty argument cannot run, got {other:?}"),
    }
    let replay = runner.store().replay(session).await.expect("replay");
    let refusal = replay
        .iter()
        .find_map(|entry| match &entry.event {
            SessionEvent::ToolResult {
                call_id, output, ..
            } if call_id == "c1" => Some(output.clone()),
            _ => None,
        })
        .expect("the call is answered");
    assert!(
        refusal.to_string().contains("cmd (string) required"),
        "got {refusal}"
    );
}

/// A reply cut at the cap carries a complete call beside the fragment the cut
/// left mid-arguments. Only the fragment goes: a drop that took the whole reply
/// with it would pass a check that only looks for the fragment's absence.
#[tokio::test]
async fn test_cut_call_dropped() {
    let p = Arc::new(ScriptRawProvider::new(vec![
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t".into() },
            LlmEvent::TextDelta {
                id: "t".into(),
                text: "partial thought".into(),
            },
            LlmEvent::TextEnd { id: "t".into() },
            LlmEvent::ToolCall {
                id: "whole-1".into(),
                name: "plain".into(),
                input: json!({"cmd": "ls"}),
            },
            LlmEvent::ToolCall {
                id: "cut-1".into(),
                name: "arg".into(),
                input: json!({}),
            },
            LlmEvent::Finish {
                reason: "length".into(),
                usage: None,
            },
        ],
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t".into() },
            LlmEvent::TextDelta {
                id: "t".into(),
                text: "done".into(),
            },
            LlmEvent::TextEnd { id: "t".into() },
            LlmEvent::Finish {
                reason: "stop".into(),
                usage: None,
            },
        ],
    ]));
    let runner = runner_with(p, arg_registry());
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.expect("run");
    let replay = runner.store().replay(session).await.expect("replay");
    let calls: Vec<String> = replay
        .iter()
        .filter_map(|entry| match &entry.event {
            SessionEvent::ToolCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !calls.iter().any(|id| id == "cut-1"),
        "the cut fragment must not reach the log: {calls:?}"
    );
    assert!(
        calls.iter().any(|id| id == "whole-1"),
        "the continuation's complete call must survive the drop: {calls:?}"
    );
    let texts: Vec<String> = replay
        .iter()
        .filter_map(|entry| match &entry.event {
            SessionEvent::AssistantMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().any(|text| text.contains("partial thought")),
        "the text that did arrive stays in the log: {texts:?}"
    );
}
