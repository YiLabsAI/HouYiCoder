//! Tests for this module.

use super::*;
use houyicoder_api::skill::{SkillFamily, SkillProvenance, SkillSource};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::permission::AskSource;

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
