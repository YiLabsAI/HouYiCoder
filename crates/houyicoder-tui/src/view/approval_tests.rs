//! Tests for the approval card renderer, split from approval.rs so the
//! production file stays under the file-size gate.

use super::{cap_first, diff_preview};
use crate::composition;
use crate::test_support::render_text;
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use serde_json::json;

#[test]
fn test_diff_preview_edit_red() {
    let v = json!({"path": "a.rs", "old_string": "foo()", "new_string": "bar()"});
    let lines = diff_preview("edit", &v).expect("preview");
    // one old line (-foo()) + one new line (+bar()) = 2.
    assert_eq!(lines.len(), 2);
}

#[test]
fn test_diff_preview_multiedit_multi() {
    let v = json!({"path": "a.rs", "edits": [
        {"old_string": "a", "new_string": "b"},
        {"old_string": "c", "new_string": "d"}
    ]});
    let lines = diff_preview("multiedit", &v).expect("preview");
    // 2 "edit N:" headers + 2 old + 2 new = 6.
    assert_eq!(lines.len(), 6);
}

#[test]
fn test_diff_preview_other_tool() {
    assert!(diff_preview("bash", &json!({"command": "ls"})).is_none());
}

#[test]
fn test_diff_preview_malformed_none() {
    assert!(diff_preview("edit", &json!({"path": "a.rs"})).is_none());
}

#[test]
fn test_cap_first_uppercases() {
    assert_eq!(cap_first("bash"), "Bash");
    assert_eq!(cap_first("edit"), "Edit");
}

#[test]
fn test_cap_first_empty() {
    assert_eq!(cap_first(""), "");
}

#[test]
fn test_render_no_heavy_border() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: "bash".into(),
        args: r#"{"command":"find . -type f | wc -l"}"#.into(),
        reason: "agent wants to run this tool".into(),
        selected: 0,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    // The heavy double-border box chars must not appear.
    for ch in ['╔', '╗', '╚', '╝', '║', '═'] {
        assert!(
            !out.contains(ch),
            "heavy border char '{ch}' should be gone, got:\n{out}"
        );
    }
}

#[test]
fn test_render_separator_and_question() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: "bash".into(),
        args: r#"{"command":"find . -type f | wc -l"}"#.into(),
        reason: "agent wants to run this tool".into(),
        selected: 0,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    // Thin separator line present.
    assert!(out.contains('─'), "separator line missing:\n{out}");
    // Proceed question present.
    assert!(
        out.contains("Do you want to proceed?"),
        "proceed question missing:\n{out}"
    );
    // Numbered Yes/Yes-don't-ask/No options present (aligned order).
    assert!(out.contains("1. Yes"), "Yes option missing:\n{out}");
    assert!(
        out.contains("2. Yes, and don't ask again"),
        "dont-ask option missing:\n{out}"
    );
    assert!(out.contains("3. No"), "No option missing:\n{out}");
    // Cursor marker on the focused (Yes) option.
    assert!(out.contains('❯'), "cursor marker missing:\n{out}");
    // Bottom hint present.
    assert!(out.contains("Esc cancel"), "hint line missing:\n{out}");
    // The actual command text is shown (not just raw JSON).
    assert!(
        out.contains("find . -type f | wc -l"),
        "command text missing:\n{out}"
    );
}

#[test]
fn test_render_cursor_on_reject() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: "bash".into(),
        args: r#"{"command":"ls"}"#.into(),
        reason: "test".into(),
        selected: 1,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    let lines: Vec<&str> = out.lines().collect();
    // Find the Yes line and the No line. The cursor marker should be
    // on the No line (selected=1), not on the Yes line.
    let yes_line = lines
        .iter()
        .find(|l| l.contains("1. Yes"))
        .expect("Yes line");
    let no_line = lines.iter().find(|l| l.contains("3. No")).expect("No line");
    assert!(
        !yes_line.contains('❯'),
        "cursor should not be on Yes:\n{out}"
    );
    assert!(no_line.contains('❯'), "cursor should be on No:\n{out}");
}

#[test]
fn test_render_cursor_dont_ask() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: "bash".into(),
        args: r#"{"command":"ls"}"#.into(),
        reason: "test".into(),
        selected: 2,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    let lines: Vec<&str> = out.lines().collect();
    let yes_line = lines
        .iter()
        .find(|l| l.contains("1. Yes"))
        .expect("Yes line");
    let dont_ask_line = lines
        .iter()
        .find(|l| l.contains("2. Yes, and don't ask again"))
        .expect("dont-ask line");
    assert!(
        !yes_line.contains('❯'),
        "cursor should not be on Yes:\n{out}"
    );
    assert!(
        dont_ask_line.contains('❯'),
        "cursor should be on dont-ask:\n{out}"
    );
}

/// The reason the gate produced renders as the card's reason line, so the
/// user reads why they are being asked instead of a generic prompt.
#[test]
fn test_renders_gate_reason_detail() {
    use houyicoder_protocol::frontend::permission::AskSource;
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: "bash".into(),
        args: r#"{"command":"rm -rf x"}"#.into(),
        reason: "rm needs confirmation".into(),
        source: Some(AskSource::Detection),
        selected: 0,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Detection: rm needs confirmation"),
        "source label + reason detail must render: {out}"
    );
    assert!(
        !out.contains("agent wants to run this tool"),
        "generic fallback must not show when a reason is present: {out}"
    );
}

/// A containment note renders on its own line beneath the reason.
#[test]
fn test_renders_containment_note_line() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: "bash".into(),
        args: r#"{"command":"curl x"}"#.into(),
        reason: "network egress".into(),
        containment_note: Some("the sandbox will block this".into()),
        selected: 0,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("the sandbox will block this"),
        "containment note must render on its own line: {out}"
    );
}

/// A protected-path ask (source SystemSafety) hides the "Yes, and don't
/// ask again" option and renumbers No to 2: consent cannot override a
/// safety check, so the choice is not offered.
#[test]
fn test_system_safety_hides_option() {
    use houyicoder_protocol::frontend::permission::AskSource;
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: "edit".into(),
        args: r#"{"path":".git/config"}"#.into(),
        reason: "protected path".into(),
        source: Some(AskSource::SystemSafety),
        selected: 0,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    assert!(
        !out.contains("don't ask again"),
        "remember option must be hidden for a protected-path ask: {out}"
    );
    // No is renumbered to 2 (display), not 3.
    assert!(out.contains("2. No"), "No must be renumbered to 2: {out}");
    assert!(
        !out.contains("3. No"),
        "stale 3. No must not show when remember is hidden: {out}"
    );
    // Hint reflects the two-option layout.
    assert!(
        !out.contains("1/2/3 select"),
        "three-option hint must not show when remember is hidden: {out}"
    );
    assert!(out.contains("1/2 select"), "two-option hint missing: {out}");
}

/// A two-option card (protected-path) should be vertically compact:
/// no blank line where the third option would be, and the hint line
/// immediately follows the No option. Count the non-empty lines
/// between the Yes option and the hint — they should be exactly one
/// (the No line), not two (No + blank).
#[test]
fn test_two_option_card_compact() {
    use houyicoder_protocol::frontend::permission::AskSource;
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: "edit".into(),
        args: r#"{"path":".git/config"}"#.into(),
        reason: "protected path".into(),
        source: Some(AskSource::SystemSafety),
        selected: 0,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    let lines: Vec<&str> = out.lines().collect();
    // Find the Yes line, then count non-empty lines until the hint.
    let yes_idx = lines
        .iter()
        .position(|l| l.contains("1. Yes"))
        .expect("Yes line");
    let hint_idx = lines
        .iter()
        .position(|l| l.contains("Esc cancel"))
        .expect("hint line");
    let between: usize = lines[yes_idx + 1..hint_idx]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .count();
    assert_eq!(
        between, 1,
        "exactly one option (No) between Yes and hint, got {between}:\n{out}"
    );
}

/// A long command in the args block is tail-truncated so the reason
/// and option lines below stay visible in the card's bounded area.
#[test]
fn test_long_command_truncated() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    let long_cmd = format!("echo {}", "x".repeat(120));
    app.approval = Some(crate::state::Approval {
        tool: "bash".into(),
        args: serde_json::to_string(&serde_json::json!({ "command": long_cmd })).unwrap(),
        reason: "test".into(),
        selected: 0,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains('…'),
        "truncated long command should show ellipsis: {out}"
    );
}

/// A detection-sourced ask (not SystemSafety) keeps the remember option.
#[test]
fn test_non_safety_keeps_option() {
    use houyicoder_protocol::frontend::permission::AskSource;
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: "bash".into(),
        args: r#"{"command":"rm x"}"#.into(),
        reason: "rm needs confirmation".into(),
        source: Some(AskSource::Detection),
        selected: 0,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("don't ask again"),
        "remember option must show for a non-safety ask: {out}"
    );
    assert!(
        out.contains("3. No"),
        "No must stay 3 when remember shows: {out}"
    );
}

/// The entitlement card: its own title, the skill + blocked services
/// rendered as lines, the persistent authorize question, and the
/// two-option (Always allow / No) layout — no don't-ask-again.
#[test]
fn test_entitlement_card_two_option() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: ENTITLEMENT_TOOL.into(),
        args: r#"{"skill":"ego-browser","origin":"user","services":["com.citrolabs.ego.lite.ego-browser"]}"#
            .into(),
        reason: "deny-log discovery".into(),
        source: None,
        selected: 0,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Sandbox entitlement"),
        "entitlement title missing: {out}"
    );
    assert!(
        out.contains("Skill ego-browser (user) was blocked from"),
        "skill + origin line missing: {out}"
    );
    assert!(
        out.contains("com.citrolabs.ego.lite.ego-browser"),
        "service line missing: {out}"
    );
    assert!(
        out.contains("Always authorize this service for ego-browser?"),
        "persistent authorize question missing: {out}"
    );
    assert!(
        out.contains("1. Always allow"),
        "persistent allow option missing: {out}"
    );
    assert!(
        !out.contains("don't ask again"),
        "remember option must be hidden for entitlement: {out}"
    );
    assert!(
        out.contains("2. No"),
        "No must be renumbered to 2 for entitlement: {out}"
    );
    assert!(
        out.contains("1/2 select"),
        "two-option hint missing for entitlement: {out}"
    );
    assert!(
        !out.contains("Entitlement command"),
        "generic tool-command title must not show for entitlement: {out}"
    );
}

/// The entitlement detail includes the triggering command, truncated
/// when the path is long (temp paths are very long).
#[test]
fn test_entitlement_detail_command_line() {
    let long_cmd = format!("/tmp/houyi-entitlement-repo-123456/{}", "x".repeat(80));
    let args = serde_json::json!({
        "skill": "ego-browser",
        "origin": "user",
        "services": ["com.citrolabs.ego.lite.ego-browser"],
        "command": long_cmd,
    })
    .to_string();
    let lines = super::entitlement_detail(&args, serde_json::from_str(&args).ok().as_ref());
    let text: String = lines.iter().map(|l| l.to_string()).collect();
    assert!(
        text.contains("Command:"),
        "command line must be in detail: {text}"
    );
    assert!(
        text.contains('…'),
        "long command must be truncated with ellipsis: {text}"
    );
}

/// A multi-byte command at the 60-byte boundary must not panic.
/// Without floor_char_boundary, slicing at a byte offset that lands
/// inside a multi-byte character would panic.
#[test]
fn test_entitlement_detail_multibyte_safe() {
    // Emoji are 4 bytes each in UTF-8. 15 emoji = 60 bytes, so the
    // 60-byte boundary lands exactly at a char boundary. Add one
    // more byte's worth to force a mid-character split.
    let cmd = "\u{1F600}".repeat(15) + "x";
    let args = serde_json::json!({
        "skill": "ego-browser",
        "origin": "user",
        "services": ["x.y.z"],
        "command": cmd,
    })
    .to_string();
    // Must not panic — that is the entire assertion.
    let _lines = super::entitlement_detail(&args, serde_json::from_str(&args).ok().as_ref());
}
