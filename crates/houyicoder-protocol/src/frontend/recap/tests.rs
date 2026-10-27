//! Tests for this module.

use super::*;
use serde_json::json;

#[test]
fn test_focus_round_trips() {
    let params = FocusReport { focused: false }.params();
    assert_eq!(params, json!({ "focused": false }));
    let report: FocusReport = serde_json::from_value(params).expect("the report decodes");
    assert!(!report.focused);
}

/// A payload whose field name drifted does not read as an unfocused
/// terminal: the host gets no report at all rather than a wrong one.
#[test]
fn test_focus_needs_field() {
    assert!(serde_json::from_value::<FocusReport>(json!({ "focus": true })).is_err());
    assert!(serde_json::from_value::<FocusReport>(json!({})).is_err());
}

#[test]
fn test_recap_round_trips() {
    let params = RecapNotice {
        text: "修构建。下一步：重跑失败的测试。".into(),
    }
    .params();
    assert_eq!(
        params,
        json!({ "text": "修构建。下一步：重跑失败的测试。" })
    );
    let notice: RecapNotice = serde_json::from_value(params).expect("the notice decodes");
    assert_eq!(notice.text, "修构建。下一步：重跑失败的测试。");
}

/// An empty text is a shape the type accepts; refusing empty answers is the
/// host's rule for what it writes, not the payload's rule for what it carries.
#[test]
fn test_recap_accepts_empty() {
    let notice: RecapNotice = serde_json::from_value(json!({ "text": "" })).expect("decodes");
    assert!(notice.text.is_empty());
}
