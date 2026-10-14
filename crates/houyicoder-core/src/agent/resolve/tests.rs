//! Tests for this module.

use super::*;
use houyicoder_api::skill::{SkillFamily, SkillProvenance, SkillSource};

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
