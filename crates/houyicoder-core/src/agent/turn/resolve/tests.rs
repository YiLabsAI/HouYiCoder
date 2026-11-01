//! Tests for this module.

use super::*;
use houyicoder_api::skill::{SkillFamily, SkillProvenance, SkillSource};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::permission::AskSource;
use std::sync::Mutex;

fn subject(skill: &str) -> GrantSubject {
    SkillSource::new(SkillFamily::Houyi, SkillProvenance::UserHome).grant_subject(skill)
}

fn bash_names(ids: &[&str]) -> HashMap<String, String> {
    ids.iter()
        .map(|i| (i.to_string(), "bash".to_string()))
        .collect()
}

/// An empty exec list (no commands mapped).
fn empty_exec() -> Vec<ToolCallPlan> {
    Vec::new()
}

#[test]
fn test_scan_finds_services() {
    let results = vec![
        ("call-1".into(), serde_json::json!({"success": true})),
        (
            "call-2".into(),
            serde_json::json!({"success": false, "authorizable_services": ["x.y.z"]}),
        ),
    ];
    let req = scan_for_authorizable(
        &results,
        &bash_names(&["call-1", "call-2"]),
        &subject("ego-browser"),
        "user",
        &empty_exec(),
    );
    let req = req.expect("bash result with services raises");
    assert_eq!(req.tool_name, ENTITLEMENT_TOOL);
    assert!(req.call_id.starts_with("entitlement-"));
    assert!(req.call_id.ends_with("-ego-browser"));
    assert_eq!(req.input["skill"], "ego-browser");
    assert_eq!(req.input["origin"], "user");
}

#[test]
fn test_scan_ids_unique() {
    let results = vec![(
        "call-1".into(),
        serde_json::json!({"authorizable_services": ["a.b"]}),
    )];
    let a = scan_for_authorizable(
        &results,
        &bash_names(&["call-1"]),
        &subject("s"),
        "user",
        &empty_exec(),
    )
    .unwrap();
    let b = scan_for_authorizable(
        &results,
        &bash_names(&["call-1"]),
        &subject("s"),
        "user",
        &empty_exec(),
    )
    .unwrap();
    assert_ne!(a.call_id, b.call_id, "repeated raises must not collide");
}

#[test]
fn test_scan_ignores_non_bash() {
    let results = vec![(
        "call-1".into(),
        serde_json::json!({"authorizable_services": ["x.y.z"]}),
    )];
    let names = HashMap::from([("call-1".to_string(), "grep".to_string())]);
    assert!(
        scan_for_authorizable(
            &results,
            &names,
            &subject("ego-browser"),
            "user",
            &empty_exec(),
        )
        .is_none()
    );
}

#[test]
fn test_scan_empty_services() {
    let results = vec![(
        "call-1".into(),
        serde_json::json!({"authorizable_services": []}),
    )];
    assert!(
        scan_for_authorizable(
            &results,
            &bash_names(&["call-1"]),
            &subject("ego-browser"),
            "user",
            &empty_exec()
        )
        .is_none()
    );
}

#[test]
fn test_scan_no_field() {
    let results = vec![("call-1".into(), serde_json::json!({"success": false}))];
    assert!(
        scan_for_authorizable(
            &results,
            &bash_names(&["call-1"]),
            &subject("ego-browser"),
            "user",
            &empty_exec()
        )
        .is_none()
    );
}

/// A tool standing in for a guarded one: it names a cause for its ask and
/// declares one required argument, the way a real tool's gate does.
struct AskingTool;

impl Tool for AskingTool {
    fn name(&self) -> &str {
        "ask_tool"
    }
    fn description(&self) -> &str {
        ""
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {"path": {"type": "string"}},
        })
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(Value::Null) })
    }
    /// Names a cause for every input, including one that cannot run: the
    /// plan, not the tool, is what withholds the ask.
    fn approval_reason(&self, _input: &Value) -> Option<AskReason> {
        Some(AskReason {
            source: AskSource::Detection,
            validator: "path-bounds".to_string(),
            detail: "outside the workspace".to_string(),
            containment_note: None,
        })
    }
}

/// A tool that calls itself parallel-safe while carrying a name the check rule
/// reads in emission order. A guest could register one; the plan must not
/// believe it.
struct ParallelNamedTool(&'static str);

impl Tool for ParallelNamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        ""
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(Value::Null) })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

/// The check rule reads a shell call against the edits before it in emission
/// order, and a parallel batch has no order. So a call of either family runs
/// alone however its own tool answers; a name the rule does not read keeps the
/// tool's answer.
#[test]
fn test_check_families_run_alone() {
    for name in ["bash", "sh", "Edit", "multiedit", "patch"] {
        let plan = ToolCallPlan::new(
            "call-1".into(),
            Arc::new(ParallelNamedTool(name)),
            serde_json::json!({}),
        );
        assert!(!plan.concurrency_safe, "{name} runs beside nothing");
    }
    let plan = ToolCallPlan::new(
        "call-2".into(),
        Arc::new(ParallelNamedTool("grep")),
        serde_json::json!({}),
    );
    assert!(
        plan.concurrency_safe,
        "a read-only tool keeps its own answer"
    );
}

/// The plan carries the cause the tool reported, so the approval card built
/// from the interruption explains the call without judging it again.
#[test]
fn test_plan_carries_ask_reason() {
    let plan = ToolCallPlan::new(
        "call-1".into(),
        Arc::new(AskingTool),
        serde_json::json!({"path": "/etc/hosts"}),
    );
    let approval = plan.approval.expect("the plan carries the ask");
    assert_eq!(approval.detail, "outside the workspace");
    assert_eq!(approval.validator, "path-bounds");
}

/// A call that misses its required argument is answered with the schema error
/// instead of being put to a human, so it reaches no approval request even
/// when the tool names a cause for every input. An empty string counts as
/// missing unless the tool declares that field as one it accepts empty.
#[test]
fn test_plan_withholds_ask_missing() {
    for input in [serde_json::json!({}), serde_json::json!({"path": ""})] {
        let plan = ToolCallPlan::new("call-1".into(), Arc::new(AskingTool), input.clone());
        assert!(
            plan.approval.is_none(),
            "a call that cannot run is not put to a human: {input}"
        );
        assert_eq!(plan.invalid_arguments, ["path (string)"], "{input}");
    }
}

/// A tool whose host runs a translated command: the stub records what its
/// approval question was asked about, standing in for the gate a real tool
/// consults.
struct TranslatingTool {
    judged: Mutex<Option<Value>>,
}

impl Tool for TranslatingTool {
    fn name(&self) -> &str {
        "translating_tool"
    }
    fn description(&self) -> &str {
        ""
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(Value::Null) })
    }
    fn resolved_input(&self, _input: &Value) -> Option<Value> {
        Some(serde_json::json!({"command": "run >/dev/null"}))
    }
    fn approval_reason(&self, input: &Value) -> Option<AskReason> {
        *self.judged.lock().expect("judged lock") = Some(input.clone());
        None
    }
}

/// The translation resolves before the call is judged: the approval question
/// and the execution both address the text the host runs. A translation
/// applied after the verdict would leave the gate judging text the shell never
/// receives, so this asserts what the tool's question saw, not only what the
/// plan carries.
#[test]
fn test_plan_judges_translated_input() {
    let tool = Arc::new(TranslatingTool {
        judged: Mutex::new(None),
    });
    let plan = ToolCallPlan::new(
        "call-1".into(),
        tool.clone(),
        serde_json::json!({"command": "run >nul"}),
    );
    let judged = tool
        .judged
        .lock()
        .expect("judged lock")
        .clone()
        .expect("the tool was asked about its call");
    assert_eq!(
        judged["command"], "run >/dev/null",
        "the verdict addresses the command the host runs"
    );
    assert_eq!(
        plan.input["command"], "run >/dev/null",
        "execution runs the command the verdict addressed"
    );
}
