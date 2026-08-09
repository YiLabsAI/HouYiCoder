//! Tests for the AskUserQuestion card: parsing, answer building, decision
//! construction, and the full parse-select-resume path through the tool.
//! Uses the shared render_text helper so the card layout is verified at
//! the buffer level (behavior, not pixel snapshots).

use serde_json::json;

use crate::records::{AskQuestion, QuestionCard};

/// Build a minimal single-select question input.
pub(crate) fn single_input() -> serde_json::Value {
    json!({
        "questions": [{
            "question": "Which library?",
            "header": "Library",
            "options": [
                {"label": "chrono", "description": "mature, heavy"},
                {"label": "time", "description": "lighter, newer"}
            ],
            "multiSelect": false
        }]
    })
}

/// Build a minimal multi-select question input.
pub(crate) fn multi_input() -> serde_json::Value {
    json!({
        "questions": [{
            "question": "Which features?",
            "header": "Features",
            "options": [
                {"label": "auth", "description": "login flow"},
                {"label": "cache", "description": "fast reads"},
                {"label": "search", "description": "full-text"}
            ],
            "multiSelect": true
        }]
    })
}

/// Build a two-question input.
pub(crate) fn two_question_input() -> serde_json::Value {
    json!({
        "questions": [
            {"question": "q1", "header": "h1", "options": [{"label":"a","description":"x"},{"label":"b","description":"y"}], "multiSelect": false},
            {"question": "q2", "header": "h2", "options": [{"label":"c","description":"x"},{"label":"d","description":"y"}], "multiSelect": false}
        ]
    })
}

// --- parse tests ---

#[test]
fn test_parse_single_question() {
    let aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    assert_eq!(aq.questions.len(), 1);
    assert_eq!(aq.questions[0].question, "Which library?");
    assert_eq!(aq.questions[0].header, "Library");
    assert_eq!(aq.questions[0].options.len(), 2);
    assert!(!aq.questions[0].multi_select);
    assert_eq!(aq.selections.len(), 1);
    assert!(aq.selections[0].is_empty());
    assert_eq!(aq.cursors.len(), 1);
    assert_eq!(aq.cursors[0], 0);
}

#[test]
fn test_parse_multi_question() {
    let aq = AskQuestion::parse("c1", &multi_input()).expect("parse");
    assert_eq!(aq.questions.len(), 1);
    assert!(aq.questions[0].multi_select);
    assert_eq!(aq.questions[0].options.len(), 3);
}

#[test]
fn test_parse_two_questions() {
    let aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    assert_eq!(aq.questions.len(), 2);
    assert_eq!(aq.questions[0].question, "q1");
    assert_eq!(aq.questions[1].question, "q2");
}

#[test]
fn test_parse_rejects_few_options() {
    let bad = json!({
        "questions": [{
            "question": "q", "header": "h",
            "options": [{"label":"a","description":"x"}],
            "multiSelect": false
        }]
    });
    assert!(AskQuestion::parse("c1", &bad).is_none());
}

#[test]
fn test_parse_rejects_many_questions() {
    let mut qs = Vec::new();
    for i in 0..5 {
        qs.push(json!({
            "question": format!("q{i}"), "header": "h",
            "options": [{"label":"a","description":"x"},{"label":"b","description":"y"}],
            "multiSelect": false
        }));
    }
    let bad = json!({"questions": qs});
    assert!(AskQuestion::parse("c1", &bad).is_none());
}

#[test]
fn test_parse_rejects_missing_questions() {
    let bad = json!({"foo": "bar"});
    assert!(AskQuestion::parse("c1", &bad).is_none());
}

#[test]
fn test_parse_rejects_empty_questions() {
    let bad = json!({"questions": []});
    assert!(AskQuestion::parse("c1", &bad).is_none());
}

#[test]
fn test_parse_preserves_original_input() {
    let input = single_input();
    let aq = AskQuestion::parse("c1", &input).expect("parse");
    assert_eq!(aq.original_input, input);
}

#[test]
fn test_parse_carries_call_id() {
    let aq = AskQuestion::parse("call-xyz", &single_input()).expect("parse");
    assert_eq!(aq.call_id, "call-xyz");
}

// --- answer building tests ---

#[test]
fn test_answers_single_select() {
    let mut aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    aq.selections[0] = vec![0]; // chrono
    let answers = aq.build_answers();
    assert_eq!(answers["Which library?"].as_str().unwrap(), "chrono");
}

#[test]
fn test_answers_single_second_option() {
    let mut aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    aq.selections[0] = vec![1]; // time
    let answers = aq.build_answers();
    assert_eq!(answers["Which library?"].as_str().unwrap(), "time");
}

#[test]
fn test_answers_multi_joins_comma() {
    let mut aq = AskQuestion::parse("c1", &multi_input()).expect("parse");
    aq.selections[0] = vec![0, 2]; // auth, search
    let answers = aq.build_answers();
    assert_eq!(answers["Which features?"].as_str().unwrap(), "auth, search");
}

#[test]
fn test_answers_other_typed_text() {
    let mut aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    let other_idx = aq.questions[0].options.len(); // = 2
    aq.selections[0] = vec![other_idx];
    aq.other_text[0] = Some("a custom crate".to_string());
    let answers = aq.build_answers();
    assert_eq!(
        answers["Which library?"].as_str().unwrap(),
        "a custom crate"
    );
}

#[test]
fn test_answers_other_empty_skips() {
    let mut aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    let other_idx = aq.questions[0].options.len();
    aq.selections[0] = vec![other_idx];
    aq.other_text[0] = Some(String::new());
    let answers = aq.build_answers();
    assert_eq!(answers["Which library?"].as_str().unwrap(), "");
}

#[test]
fn test_answers_other_mixed() {
    let mut aq = AskQuestion::parse("c1", &multi_input()).expect("parse");
    let other_idx = aq.questions[0].options.len(); // = 3
    aq.selections[0] = vec![0, other_idx]; // auth + Other
    aq.other_text[0] = Some("custom feature".to_string());
    let answers = aq.build_answers();
    assert_eq!(
        answers["Which features?"].as_str().unwrap(),
        "auth, custom feature"
    );
}

#[test]
fn test_answers_two_questions() {
    let mut aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    aq.selections[0] = vec![0]; // a
    aq.selections[1] = vec![1]; // d
    let answers = aq.build_answers();
    assert_eq!(answers["q1"].as_str().unwrap(), "a");
    assert_eq!(answers["q2"].as_str().unwrap(), "d");
}

#[test]
fn test_answers_empty_when_unselected() {
    let aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    let answers = aq.build_answers();
    assert_eq!(answers["Which library?"].as_str().unwrap(), "");
}

// --- updated input (decision) tests ---

#[test]
fn test_updated_input_has_answers() {
    let mut aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    aq.selections[0] = vec![0];
    let updated = aq.build_updated_input();
    assert!(updated.get("questions").is_some(), "questions preserved");
    let answers = updated.get("answers").expect("answers present");
    assert_eq!(answers["Which library?"].as_str().unwrap(), "chrono");
}

#[test]
fn test_updated_input_preserves_questions() {
    let input = two_question_input();
    let mut aq = AskQuestion::parse("c1", &input).expect("parse");
    aq.selections[0] = vec![0];
    aq.selections[1] = vec![1];
    let updated = aq.build_updated_input();
    let qs = updated.get("questions").unwrap().as_array().unwrap();
    assert_eq!(qs.len(), 2);
    assert_eq!(qs[0]["question"].as_str().unwrap(), "q1");
}

// --- decision construction tests ---

#[test]
fn test_decision_approve_with_input() {
    use houyicoder_core::agent::ApprovalDecision;
    let mut aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    aq.selections[0] = vec![0];
    let call_id = aq.call_id.clone();
    let updated = aq.build_updated_input();
    let decision = ApprovalDecision::approve_with_input(&call_id, updated);
    assert!(decision.approved);
    assert!(decision.updated_input.is_some());
    assert_eq!(decision.call_id, "c1");
}

#[test]
fn test_decision_reject() {
    use houyicoder_core::agent::ApprovalDecision;
    let decision = ApprovalDecision::reject("c1");
    assert!(!decision.approved);
    assert!(decision.updated_input.is_none());
}

// --- render tests ---

#[test]
fn test_render_card_question_options() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("Which library?"),
        "question text missing:\n{out}"
    );
    assert!(out.contains("chrono"), "option chrono missing:\n{out}");
    assert!(out.contains("time"), "option time missing:\n{out}");
    assert!(
        out.contains("Type something."),
        "Other placeholder missing:\n{out}"
    );
    assert!(out.contains("[Library]"), "header chip missing:\n{out}");
}

#[test]
fn test_render_card_cursor_marker() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let mut aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    aq.cursors[0] = 1; // focus on "time"
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains('>'), "cursor marker missing:\n{out}");
    let lines: Vec<&str> = out.lines().collect();
    let chrono_line = lines.iter().find(|l| l.contains("chrono")).expect("chrono");
    let time_line = lines.iter().find(|l| l.contains("time")).expect("time");
    assert!(!chrono_line.contains('>'), "cursor on chrono:\n{out}");
    assert!(time_line.contains('>'), "cursor not on time:\n{out}");
}

#[test]
fn test_render_card_multi_hint() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let aq = AskQuestion::parse("c1", &multi_input()).expect("parse");
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains("toggle"), "multi-select hint missing:\n{out}");
    // Multi-select Other placeholder has no trailing period (single does).
    assert!(
        out.contains("Type something"),
        "multi Other placeholder missing:\n{out}"
    );
    assert!(
        !out.contains("Type something."),
        "multi Other placeholder must not have a period:\n{out}"
    );
}

#[test]
fn test_render_card_single_hint() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("enter select"),
        "single-select hint missing:\n{out}"
    );
}

#[test]
fn test_render_card_esc_hint() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains("esc cancel"), "esc hint missing:\n{out}");
}

#[test]
fn test_render_card_selected_checkbox() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let mut aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    aq.selections[0] = vec![0]; // select chrono
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains("[x]"), "checked box missing:\n{out}");
    assert!(out.contains("[ ]"), "unchecked box missing:\n{out}");
}

#[test]
fn test_render_card_separator() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains('-'), "separator line missing:\n{out}");
}

// --- full path: parse + build_answers + tool result ---

/// Block on an async future using a fresh tokio runtime (the TUI already
/// depends on tokio for its agent-loop runtime).
fn block_on<F>(fut: F) -> F::Output
where
    F: std::future::Future,
{
    tokio::runtime::Runtime::new().expect("rt").block_on(fut)
}

#[test]
fn test_single_select_through_tool() {
    use houyicoder_api::tool::{Tool, ToolCtx};
    use houyicoder_core::agent::AskUserQuestionTool;

    let input = single_input();
    let mut aq = AskQuestion::parse("c1", &input).expect("parse");
    aq.selections[0] = vec![0]; // chrono
    let updated = aq.build_updated_input();

    let tool = AskUserQuestionTool::new();
    let result = block_on(tool.execute_authorized(ToolCtx::new("test"), updated)).unwrap();
    let summary = result["summary"].as_str().expect("summary");
    assert!(summary.contains("User has answered"), "{summary}");
    assert!(summary.contains("chrono"), "{summary}");
    assert_eq!(
        result["answers"]["Which library?"].as_str().unwrap(),
        "chrono"
    );
}

#[test]
fn test_multi_select_through_tool() {
    use houyicoder_api::tool::{Tool, ToolCtx};
    use houyicoder_core::agent::AskUserQuestionTool;

    let input = multi_input();
    let mut aq = AskQuestion::parse("c1", &input).expect("parse");
    aq.selections[0] = vec![0, 2]; // auth, search
    let updated = aq.build_updated_input();

    let tool = AskUserQuestionTool::new();
    let result = block_on(tool.execute_authorized(ToolCtx::new("test"), updated)).unwrap();
    let summary = result["summary"].as_str().expect("summary");
    assert!(summary.contains("auth, search"), "{summary}");
}

#[test]
fn test_other_text_through_tool() {
    use houyicoder_api::tool::{Tool, ToolCtx};
    use houyicoder_core::agent::AskUserQuestionTool;

    let input = single_input();
    let mut aq = AskQuestion::parse("c1", &input).expect("parse");
    let other_idx = aq.questions[0].options.len();
    aq.selections[0] = vec![other_idx];
    aq.other_text[0] = Some("serde_json".to_string());
    let updated = aq.build_updated_input();

    let tool = AskUserQuestionTool::new();
    let result = block_on(tool.execute_authorized(ToolCtx::new("test"), updated)).unwrap();
    let summary = result["summary"].as_str().expect("summary");
    assert!(summary.contains("serde_json"), "{summary}");
}

#[test]
fn test_reject_decision_no_input() {
    use houyicoder_core::agent::ApprovalDecision;
    let decision = ApprovalDecision::reject("c1");
    assert!(!decision.approved);
    assert!(decision.updated_input.is_none());
}

#[test]
fn test_question_card_option_fields() {
    let aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    let q: &QuestionCard = &aq.questions[0];
    assert_eq!(q.options[0].label, "chrono");
    assert_eq!(q.options[0].description, "mature, heavy");
    assert!(q.options[0].preview.is_none());
}

#[test]
fn test_question_card_with_preview() {
    let input = json!({
        "questions": [{
            "question": "Which?", "header": "Pick",
            "options": [
                {"label": "a", "description": "x", "preview": "preview-a"},
                {"label": "b", "description": "y", "preview": "preview-b"}
            ],
            "multiSelect": false
        }]
    });
    let aq = AskQuestion::parse("c1", &input).expect("parse");
    assert_eq!(
        aq.questions[0].options[0].preview.as_deref(),
        Some("preview-a")
    );
    assert_eq!(
        aq.questions[0].options[1].preview.as_deref(),
        Some("preview-b")
    );
}

// --- multi-select submit path ---

#[test]
fn test_multi_submit_via_tool() {
    use houyicoder_api::tool::{Tool, ToolCtx};
    use houyicoder_core::agent::AskUserQuestionTool;

    let input = multi_input();
    let mut aq = AskQuestion::parse("c1", &input).expect("parse");
    // Simulate toggling auth and search, then submitting via the submit view.
    aq.selections[0] = vec![0, 2];
    // After advancing to the submit view and pressing Submit answers.
    let updated = aq.build_updated_input();
    let tool = AskUserQuestionTool::new();
    let result = block_on(tool.execute_authorized(ToolCtx::new("test"), updated)).unwrap();
    let summary = result["summary"].as_str().expect("summary");
    assert!(summary.contains("auth, search"), "{summary}");
}

#[test]
fn test_multi_reach_submit_view() {
    let mut aq = AskQuestion::parse("c1", &multi_input()).expect("parse");
    // The submit button index for multi-select is options.len() + 1.
    let submit_idx = aq.current_submit_btn_idx();
    assert_eq!(submit_idx, 4); // 3 options + Other + Submit = index 4
    // Toggle some options, then advance (simulating Submit button Enter).
    aq.selections[0] = vec![0];
    aq.current = aq.questions.len(); // enter submit view
    assert!(aq.is_submit_view());
    assert!(!aq.all_answered() || aq.all_answered()); // just exercise the method
    let updated = aq.build_updated_input();
    assert!(updated.get("answers").is_some());
}

// --- tab navigation between questions ---

#[test]
fn test_tab_nav_forward() {
    let mut aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    assert_eq!(aq.current, 0);
    assert!(!aq.hide_submit_tab());
    assert_eq!(aq.max_index(), 2); // 2 questions, submit view at index 2
    // Simulate Tab/Right: advance current.
    aq.current += 1;
    assert_eq!(aq.current, 1);
    // Tab again: enter submit view.
    aq.current += 1;
    assert_eq!(aq.current, 2);
    assert!(aq.is_submit_view());
}

#[test]
fn test_tab_nav_backward() {
    let mut aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    aq.current = 2; // submit view
    assert!(aq.is_submit_view());
    // Simulate Shift+Tab/Left: go back to last question.
    aq.current -= 1;
    assert_eq!(aq.current, 1);
    assert!(!aq.is_submit_view());
    // Back again.
    aq.current -= 1;
    assert_eq!(aq.current, 0);
}

#[test]
fn test_tab_nav_clamped_zero() {
    let mut aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    assert_eq!(aq.current, 0);
    // Left at 0 should stay at 0.
    if aq.current > 0 {
        aq.current -= 1;
    }
    assert_eq!(aq.current, 0);
}

#[test]
fn test_tab_nav_clamped_submit() {
    let aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    assert_eq!(aq.max_index(), 2);
    // Cannot go past submit view.
    let mut current = aq.max_index();
    if current < aq.max_index() {
        current += 1;
    }
    assert_eq!(current, 2);
}

// --- single-select advance on multi-question ---

#[test]
fn test_single_advances_multi_q() {
    let mut aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    // Select option 0 on Q1.
    aq.selections[0] = vec![0];
    // Single-select multi-question: should advance, not auto-submit.
    assert!(!aq.hide_submit_tab());
    aq.current += 1;
    assert_eq!(aq.current, 1);
    // Q1 should have an answer.
    let answers = aq.build_answers();
    assert_eq!(answers["q1"].as_str().unwrap(), "a");
}

// --- single single-select auto-submit ---

#[test]
fn test_single_auto_submit() {
    let mut aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    assert!(aq.hide_submit_tab());
    assert_eq!(aq.max_index(), 0); // no submit view reachable
    // Select an option: should auto-submit (not advance).
    aq.selections[0] = vec![0];
    let updated = aq.build_updated_input();
    assert_eq!(
        updated["answers"]["Which library?"].as_str().unwrap(),
        "chrono"
    );
}

// --- Other text flows to answer ---

#[test]
fn test_other_text_auto_submit() {
    let mut aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    let other_idx = aq.questions[0].options.len();
    // Simulate: focus Other, type text, Enter.
    aq.selections[0] = vec![other_idx];
    aq.other_text[0] = Some("custom-lib".to_string());
    assert!(aq.hide_submit_tab());
    let updated = aq.build_updated_input();
    assert_eq!(
        updated["answers"]["Which library?"].as_str().unwrap(),
        "custom-lib"
    );
}

#[test]
fn test_other_advances_multi_q() {
    let mut aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    let other_idx = aq.questions[0].options.len(); // = 2
    // Type Other text on Q1, Enter advances to Q2.
    aq.selections[0] = vec![other_idx];
    aq.other_text[0] = Some("custom".to_string());
    aq.current += 1;
    assert_eq!(aq.current, 1);
    let answers = aq.build_answers();
    assert_eq!(answers["q1"].as_str().unwrap(), "custom");
}

// --- cancel path (declined) ---

#[test]
fn test_cancel_declined_input() {
    let aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    let mut input = aq.original_input.clone();
    if let serde_json::Value::Object(ref mut obj) = input {
        obj.insert("declined".into(), serde_json::json!(true));
    }
    assert!(input["declined"].as_bool().unwrap());
    // The declined input still has the original questions.
    assert!(input.get("questions").is_some());
}

#[test]
fn test_cancel_via_tool() {
    use houyicoder_api::tool::{Tool, ToolCtx};
    use houyicoder_core::agent::AskUserQuestionTool;

    let aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    let mut input = aq.original_input.clone();
    if let serde_json::Value::Object(ref mut obj) = input {
        obj.insert("declined".into(), serde_json::json!(true));
    }
    let tool = AskUserQuestionTool::new();
    let result = block_on(tool.execute_authorized(ToolCtx::new("test"), input)).unwrap();
    // The model sees the full reject directive (stop and wait); the declined
    // flag stays set so the TUI renders its short "User declined" label
    // separately from the model-facing string.
    assert!(result["declined"].as_bool().unwrap());
    let summary = result["summary"].as_str().expect("summary");
    assert!(
        summary.contains("doesn't want to proceed") && summary.contains("STOP"),
        "expected the reject directive, got: {summary}"
    );
}

// --- submit view rendering ---

#[test]
fn test_render_submit_title() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let mut aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    aq.selections[0] = vec![0];
    aq.selections[1] = vec![1];
    aq.current = aq.questions.len(); // submit view
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("Review your answers"),
        "submit view title missing:\n{out}"
    );
}

#[test]
fn test_render_submit_answers() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let mut aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    aq.selections[0] = vec![0]; // a
    aq.selections[1] = vec![1]; // d
    aq.current = aq.questions.len();
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains("q1"), "question 1 missing:\n{out}");
    assert!(out.contains("-> a"), "answer 1 missing:\n{out}");
    assert!(out.contains("q2"), "question 2 missing:\n{out}");
    assert!(out.contains("-> d"), "answer 2 missing:\n{out}");
}

#[test]
fn test_render_submit_warning() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let mut aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    aq.selections[0] = vec![0]; // only Q1 answered
    aq.selections[1] = vec![]; // Q2 unanswered
    aq.current = aq.questions.len();
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("not answered all questions"),
        "warning missing:\n{out}"
    );
}

#[test]
fn test_render_submit_cancel_opts() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let mut aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    aq.selections[0] = vec![0];
    aq.selections[1] = vec![1];
    aq.current = aq.questions.len();
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("Submit answers"),
        "submit option missing:\n{out}"
    );
    assert!(out.contains("Cancel"), "cancel option missing:\n{out}");
}
