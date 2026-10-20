//! Peer tests for records.rs — the semantic-error-judgment tests split out
//! so records.rs stays under the file-size gate.

use crate::records::subagent_line;
use crate::records::{ContextDrillDown, ContextSkillEntry, ContextView, context_grid_text};
use crate::records::{ToolOutcome, TranscriptLine};
use crate::toggle_hint::ToggleHint;
use crate::view::context_view::render_as_rows;
use houyicoder_protocol::frontend::context::{ContextBreakdown, GridCell};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

// A non-zero exit is not always an error. grep exits 1 when no matches are
// found; diff exits 1 when files differ. Both are the command succeeding at
// its semantic job, not failing. Without this the grep no-match result
// renders red — the user sees a "failure" that is the command's normal
// output.
#[test]
fn test_grep_no_match_success() {
    let out = serde_json::json!({
        "stdout": "",
        "stderr": "",
        "exit_code": 1,
        "success": false,
    });
    let input = serde_json::json!({"command": "grep foo bar.txt"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Success,
        "grep exit 1 (no match) is semantic success"
    );
}

#[test]
fn test_diff_differ_success() {
    let out = serde_json::json!({
        "stdout": "1c1\n< a\n---\n> b\n",
        "stderr": "",
        "exit_code": 1,
        "success": false,
    });
    let input = serde_json::json!({"command": "diff a.txt b.txt"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Success,
        "diff exit 1 (files differ) is semantic success"
    );
}

#[test]
fn test_grep_exit_two_error() {
    // grep exit 2 is a real error (file not found, bad option). Only
    // exit 1 (no match) is semantic success.
    let out = serde_json::json!({
        "stdout": "",
        "stderr": "grep: bar.txt: No such file",
        "exit_code": 2,
        "success": false,
    });
    let input = serde_json::json!({"command": "grep foo bar.txt"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Error,
        "grep exit 2 is a real error"
    );
}

#[test]
fn test_false_command_is_error() {
    // The false command exits 1 with no semantic success — it is a real failure.
    let out = serde_json::json!({
        "stdout": "",
        "stderr": "",
        "exit_code": 1,
        "success": false,
    });
    let input = serde_json::json!({"command": "false"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Error,
        "false exit 1 is a real error"
    );
}

#[test]
fn test_env_prefix_grep_success() {
    // A leading env assignment (GREP_COLOR=always grep ...) must not
    // hide the grep command from semantic recognition.
    let out = serde_json::json!({
        "exit_code": 1,
        "success": false,
    });
    let input = serde_json::json!({"command": "GREP_COLOR=always grep foo bar.txt"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Success,
        "env-prefixed grep exit 1 is semantic success"
    );
}

#[test]
fn test_error_key_still_error() {
    // A non-bash tool with an error key is always an error (the
    // semantic-success rule only applies to bash).
    let out = serde_json::json!({"error": "permission denied"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "read", &Value::Null),
        ToolOutcome::Error
    );
}

/// A pipeline's exit code belongs to the LAST stage, not the first. "grep
/// foo | head" exiting 1 means head failed, not grep found no match —
/// recognizing grep here would mis-color a real head failure as success.
/// The semantic-success check must bail out on shell control operators.
#[test]
fn test_pipeline_not_semantic_success() {
    let out = serde_json::json!({
        "exit_code": 1,
        "success": false,
    });
    let input = serde_json::json!({"command": "grep foo bar.txt | head"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Error,
        "pipeline exit 1 is not auto-success even if first word is grep"
    );
}

/// A compound command (grep foo; echo done) has the exit code of the LAST
/// stage. Same bail-out as pipelines.
#[test]
fn test_compound_not_semantic_success() {
    let out = serde_json::json!({
        "exit_code": 1,
        "success": false,
    });
    let input = serde_json::json!({"command": "grep foo bar.txt; false"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Error,
        "compound command exit 1 is not auto-success"
    );
}

/// A field-level externalized child summary: content field is the marker
/// object, agentId stays top-level. The fold-group renders (agentId found)
/// and the summary falls back to the inline preview. Regression for the
/// CAS-whole-envelope bug: before field-level, the whole object was
/// replaced and agentId was destroyed.
#[test]
fn test_subagent_line_field_marker() {
    let output = serde_json::json!({
        "status": "completed",
        "content": {
            "block_ref": "a3a1dde5b075cee6",
            "preview": "found auth module in src/auth",
            "data_tag": false,
            "hint": "large output compacted; re-invoke the tool to retrieve it",
        },
        "agentId": "child-xyz",
        "color": "red",
    });
    let call_input = serde_json::json!({"subagent_type": "explore", "prompt": "find auth"});
    let line = subagent_line(&output, Some(&call_input))
        .expect("fold-group renders: agentId stays at the top level");
    match line {
        TranscriptLine::Subagent {
            child_sid,
            subagent_type,
            summary,
            color,
            ..
        } => {
            assert_eq!(child_sid, "child-xyz");
            assert_eq!(subagent_type, "explore");
            assert_eq!(summary, "found auth module in src/auth");
            assert_eq!(color.as_deref(), Some("red"));
        }
        other => panic!("expected Subagent line, got {other:?}"),
    }
}

#[test]
fn test_transcript_render_glyphs() {
    assert!(TranscriptLine::User("x".into()).render().starts_with(">"));
    assert!(TranscriptLine::Agent("hi".into()).render().starts_with("●"));
    assert!(
        TranscriptLine::System("note".into())
            .render()
            .starts_with("✻")
    );
    assert!(
        TranscriptLine::Read { path: "p".into() }
            .render()
            .contains("read")
    );
}

// I2 (call-chip slice): verbose render uses the untruncated invocation
// while folded render uses the truncated status. A search hit on a long
// command lands on the text the verbose view shows — index-equals-render
// for the call chip.
#[test]
fn test_verbose_render_uses_invocation() {
    let long = "x".repeat(300);
    let call = TranscriptLine::Tool {
        name: "bash".into(),
        tool: "bash".into(),
        status: "x".repeat(160),  // truncated chip form
        invocation: long.clone(), // untruncated
        outcome: ToolOutcome::Success,
        call_id: "c1".into(),
        body: String::new(),
        is_diff: false,
    };
    let folded = call.render();
    let verbose = call.render_verbose();
    // Folded chip carries the truncated status, not the 300-char tail.
    assert!(folded.contains(&"x".repeat(160)));
    assert!(!folded.contains(&long));
    // Verbose chip carries the full invocation.
    assert!(verbose.contains(&long));
}

#[test]
fn test_tool_chip_ellipsis() {
    let call = TranscriptLine::Tool {
        name: "bash".into(),
        tool: "bash".into(),
        status: "ego-browser nodejs with a long argument".into(),
        invocation: "ego-browser nodejs with a long argument".into(),
        outcome: ToolOutcome::Success,
        call_id: "c1".into(),
        body: String::new(),
        is_diff: false,
    };
    let rows = call.tool_call_rows(24, false).expect("tool rows");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].contains('\u{2026}'), "missing ellipsis: {:?}", rows);
    assert!(
        rows[0].ends_with(')'),
        "closing delimiter missing: {:?}",
        rows
    );
    assert!(UnicodeWidthStr::width(rows[0].as_str()) <= 24);
}

#[test]
fn test_result_body_only_result() {
    let call = TranscriptLine::Tool {
        name: "bash".into(),
        tool: "bash".into(),
        status: "ls".into(),
        invocation: "ls".into(),
        outcome: ToolOutcome::Running,
        call_id: "c1".into(),
        body: String::new(),
        is_diff: false,
    };
    assert_eq!(call.result_body(), (String::new(), false));
    let result = TranscriptLine::Tool {
        name: "result".into(),
        tool: "bash".into(),
        status: String::new(),
        invocation: String::new(),
        outcome: ToolOutcome::Success,
        call_id: "c1".into(),
        body: "ok".into(),
        is_diff: false,
    };
    assert_eq!(result.result_body(), ("ok".to_string(), false));
    // render() of a result shows the first body line under the gutter.
    assert_eq!(result.render(), "  ⎿  ok");
}

#[test]
fn test_thinking_collapses_multiline() {
    let line = TranscriptLine::Thinking {
        text: "first line\nsecond\nthird".into(),
    };
    let r = line.render();
    // Collapsed marker only (no content); the full text stays for search.
    assert!(r.contains("thinking"), "got {r}");
    assert!(r.contains("+3 lines"), "should hint 3 lines, got {r}");
    assert!(!r.contains("first line"), "content must not show, got {r}");
    // search_text returns the full text (not the collapsed render).
    assert_eq!(line.search_text(), "first line\nsecond\nthird");
}

/// The label a thought row carrying this duration renders when the caller
/// offers the expand affordance.
fn thought_label(ms: Option<u64>) -> String {
    let line = TranscriptLine::ThoughtFor {
        ms,
        reasoning: Some("considered the options".into()),
        tool_summary: None,
        turn_id: "f1".into(),
    };
    line.thought_row_text(Some(ToggleHint::Expand))
        .expect("a thought row has a label")
}

/// A turn that recorded a span under a second states it in milliseconds.
/// Whole seconds are the row's step, and rounding a measured span down to
/// that step would print a turn that took time as one that took none.
#[test]
fn test_thought_row_millisecond_span() {
    assert_eq!(
        thought_label(Some(620)),
        "✻ Thought for 620ms (ctrl+o to expand)"
    );
    assert_eq!(
        thought_label(Some(999)),
        "✻ Thought for 999ms (ctrl+o to expand)"
    );
    assert_eq!(
        thought_label(Some(0)),
        "✻ Thought for 0ms (ctrl+o to expand)"
    );
}

/// A turn with no recorded duration keeps the bare label: the log carries no
/// completion record for it, so no duration is claimed at all.
#[test]
fn test_thought_row_no_duration() {
    assert_eq!(thought_label(None), "✻ Thought (ctrl+o to expand)");
}

/// A turn of a second or more reads in whole seconds, rounded to the nearest
/// one. One second is the smallest span that still shows its number, and the
/// values either side of the next half second pin the rounding.
#[test]
fn test_thought_row_keeps_secs() {
    assert_eq!(
        thought_label(Some(1_000)),
        "✻ Thought for 1s (ctrl+o to expand)"
    );
    assert_eq!(
        thought_label(Some(1_499)),
        "✻ Thought for 1s (ctrl+o to expand)"
    );
    assert_eq!(
        thought_label(Some(1_500)),
        "✻ Thought for 2s (ctrl+o to expand)"
    );
}

#[test]
fn test_thinking_line_no_plus() {
    let line = TranscriptLine::Thinking {
        text: "only line".into(),
    };
    let r = line.render();
    // Collapsed marker only (no content); no +N hint for a single line.
    assert!(r.contains("thinking"));
    assert!(!r.contains("only line"), "content must not show, got {r}");
    assert!(!r.contains("+"));
}

/// The flattened context block is the text search reads, so a skill the
/// listing does not announce must read manual there as well: a token figure
/// would report a cost the window never carried.
#[test]
fn test_grid_text_marks_manual() {
    let view = ContextView {
        drill: ContextDrillDown {
            memory_files: Vec::new(),
            skills: vec![ContextSkillEntry {
                source: "project".into(),
                name: "brainstorm".into(),
                tokens: 0,
                announced: false,
            }],
        },
        ..Default::default()
    };
    let text = context_grid_text(&view);
    assert!(
        text.contains("brainstorm: manual"),
        "the flattened text marks the skill as the user's to invoke:\n{text}"
    );
    assert!(
        !text.contains("brainstorm: 0 tokens"),
        "the flattened text must not report a zero cost as a measurement:\n{text}"
    );
}

/// A skill row of the /context block with the origin and name under test.
fn skill_row(origin: &str, name: &str, announced: bool) -> ContextSkillEntry {
    ContextSkillEntry {
        source: origin.to_string(),
        name: name.to_string(),
        tokens: 0,
        announced,
    }
}

/// A /context block carrying the given skill rows, with one grid cell so the
/// rendered rows do not short-circuit on an empty grid.
fn view_with_skills(skills: Vec<ContextSkillEntry>) -> ContextView {
    ContextView {
        breakdown: ContextBreakdown {
            grid: vec![vec![GridCell {
                category_idx: 0,
                fullness: 1.0,
            }]],
            ..Default::default()
        },
        drill: ContextDrillDown {
            memory_files: Vec::new(),
            skills,
        },
        suggestions: Vec::new(),
    }
}

/// The flattened text reads a skill under the heading and in the position the
/// rendered rows give it: the ecosystem row arrives first in the block, and
/// the native group still leads the text, so a drag over the block lifts the
/// rows in the order the user saw.
#[test]
fn test_grid_text_share_order() {
    let view = view_with_skills(vec![
        skill_row("claude_eco", "alpha", true),
        skill_row("project", "beta", true),
    ]);
    let text = context_grid_text(&view);
    let heading = text
        .find("Native — .houyicoder/skills/")
        .expect("the native group heading is printed");
    let beta = text.find("beta: ").expect("the native row is printed");
    let alpha = text.find("alpha: ").expect("the ecosystem row is printed");
    assert!(
        heading < beta && beta < alpha,
        "the native group leads the ecosystem one:\n{text}"
    );
}

/// Both projections of the block give a skill the same cost, because the cell
/// comes from one place: a change to one view cannot leave the other
/// reporting a cost the window never carried.
#[test]
fn test_grid_text_cost_agrees() {
    let view = view_with_skills(vec![skill_row("claude_eco", "brainstorm", false)]);
    let flat = context_grid_text(&view);
    let rows: String = render_as_rows(&view, usize::MAX)
        .iter()
        .map(|(plain, _)| plain.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for text in [&flat, &rows] {
        assert!(text.contains("brainstorm: manual"), "{text}");
        assert!(!text.contains("brainstorm: 0 tokens"), "{text}");
    }
}
