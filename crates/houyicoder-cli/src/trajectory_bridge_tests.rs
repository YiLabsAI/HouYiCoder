//! The projection is pure: feed synthetic events, assert the grouped view.
//! Pins: turn grouping on TurnStarted (NOT UserInput — a prompt spans N
//! turns), token/tool aggregation, start_ms offset math, Option tokens
//! for turns without TurnUsage, and the retry count (#75/#76 UI-layer
//! regression guard — the record work is invisible in the pane without
//! these tests).

use super::*;
use houyicoder_context::{EventId, TurnEvent};

fn ev(ts: u64, kind: TurnEventKind) -> TurnEvent {
    TurnEvent {
        id: EventId::new(),
        session: SessionId::new(),
        ts,
        prev_hash: None,
        kind,
    }
}

#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
#[test]
fn test_project_groups_turn_started() {
    // One prompt + two TurnStarted = two turns. The first turn carries
    // the prompt text; the second is empty (same prompt's 2nd iteration).
    let events = vec![
        ev(
            100,
            TurnEventKind::UserInput {
                text: "hello".into(),
            },
        ),
        ev(
            105,
            TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            TurnEventKind::ToolCall {
                call_id: "c1".into(),
                tool: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
        ),
        ev(
            120,
            TurnEventKind::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({"echo": 1}),
                duration_ms: 50,
            },
        ),
        ev(
            130,
            TurnEventKind::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 1000,
                output_tokens: 500,
                cache_read_input_tokens: 800,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "test".into(),
                recovery: false,
                effort: None,
            },
        ),
        ev(
            200,
            TurnEventKind::AssistantMessage {
                text: "hi".into(),
                thinking: None,
            },
        ),
        ev(
            210,
            TurnEventKind::TurnStarted {
                turn: 2,
                call_in_turn: 0,
            },
        ),
        ev(
            220,
            TurnEventKind::ToolCall {
                call_id: "c2".into(),
                tool: "echo".into(),
                input: serde_json::json!({}),
            },
        ),
        ev(
            230,
            TurnEventKind::ToolResult {
                call_id: "c2".into(),
                output: serde_json::json!({"error": "boom"}),
                duration_ms: 10,
            },
        ),
        ev(
            240,
            TurnEventKind::TurnUsage {
                turn: 2,
                call_in_turn: 1,
                input_tokens: 2000,
                output_tokens: 100,
                cache_read_input_tokens: 1500,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "test".into(),
                recovery: false,
                effort: None,
            },
        ),
    ];
    let view = project(&events, "test");
    assert_eq!(view.total_turns, 2, "two TurnStarted => two turns");
    assert_eq!(view.tokens_in, Some(3000), "session total sums known turns");
    assert_eq!(view.tokens_out, Some(600));
    assert_eq!(view.failures, 1);
    let t1 = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(t1.n, 1);
    assert_eq!(t1.user_input, "hello", "first turn carries the prompt text");
    assert_eq!(t1.tokens_in, Some(1000));
    assert_eq!(t1.tool_count, 1);
    assert_eq!(t1.tool_fail, 0);
    assert_eq!(t1.retries, 0);
    assert_eq!(t1.duration_ms, 50);
    let t2 = match &view.rows[1] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(t2.n, 2);
    assert_eq!(t2.user_input, "", "2nd turn of the same prompt: empty");
    assert_eq!(t2.tokens_in, Some(2000));
    assert_eq!(t2.tool_fail, 1);
    // start_ms: ToolResult at ts=120 on a turn starting at TurnStarted ts=105 => 15.
    let tr = t1.events.iter().find(|e| e.kind == "tool_result").unwrap();
    assert_eq!(
        tr.start_ms, 15,
        "offset from TurnStarted.ts, not UserInput.ts"
    );
}

#[test]
fn test_multi_iteration_produces_turns() {
    // One prompt, three tool-iteration turns. Without TurnStarted
    // grouping, these would flatten to 1 row — hiding #75/#76's work.
    let events = vec![
        ev(
            100,
            TurnEventKind::UserInput {
                text: "fix the bug".into(),
            },
        ),
        ev(
            105,
            TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            120,
            TurnEventKind::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 3200,
                output_tokens: 800,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "test".into(),
                recovery: false,
                effort: None,
            },
        ),
        ev(
            300,
            TurnEventKind::TurnStarted {
                turn: 2,
                call_in_turn: 0,
            },
        ),
        ev(
            320,
            TurnEventKind::TurnUsage {
                turn: 2,
                call_in_turn: 1,
                input_tokens: 5000,
                output_tokens: 1200,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "test".into(),
                recovery: false,
                effort: None,
            },
        ),
        ev(
            500,
            TurnEventKind::TurnStarted {
                turn: 3,
                call_in_turn: 0,
            },
        ),
        ev(
            520,
            TurnEventKind::TurnUsage {
                turn: 3,
                call_in_turn: 1,
                input_tokens: 8000,
                output_tokens: 300,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "test".into(),
                recovery: false,
                effort: None,
            },
        ),
    ];
    let view = project(&events, "test");
    assert_eq!(
        view.total_turns, 3,
        "one prompt, three iterations => 3 turns"
    );
    // Only the first turn carries the prompt text.
    let t1 = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(t1.user_input, "fix the bug");
    let t2 = match &view.rows[1] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(t2.user_input, "", "2nd iteration: no new prompt");
    let t3 = match &view.rows[2] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(t3.user_input, "", "3rd iteration: no new prompt");
}

#[test]
fn test_recovery_retry_same_turn() {
    // A length-recovery retry: 2 TurnUsage events (recovery=true then
    // false) under the SAME TurnStarted. The turn count stays 1, not 2;
    // retries=1. This is the #75/#76 UI-layer regression guard — without
    // it, the retry-recording work is invisible in the pane.
    let events = vec![
        ev(
            100,
            TurnEventKind::UserInput {
                text: "long reply".into(),
            },
        ),
        ev(
            105,
            TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            TurnEventKind::AssistantMessage {
                text: "partial".into(),
                thinking: None,
            },
        ),
        ev(
            120,
            TurnEventKind::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 1000,
                output_tokens: 500,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "test".into(),
                recovery: true,
                effort: None,
            },
        ),
        ev(
            130,
            TurnEventKind::AssistantMessage {
                text: " done".into(),
                thinking: None,
            },
        ),
        ev(
            140,
            TurnEventKind::TurnUsage {
                turn: 1,
                call_in_turn: 2,
                input_tokens: 4000,
                output_tokens: 600,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "test".into(),
                recovery: false,
                effort: None,
            },
        ),
    ];
    let view = project(&events, "test");
    assert_eq!(
        view.total_turns, 1,
        "retry stays on the same turn, not a new turn"
    );
    let t = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(t.retries, 1, "one recovery=true TurnUsage => retries 1");
    // The turn's tokens come from the LAST TurnUsage (call_in_turn=2).
    assert_eq!(t.tokens_in, Some(4000));
    assert_eq!(t.tokens_out, Some(600));
}

#[test]
fn test_tokens_none_no_usage() {
    // A turn with no TurnUsage (cancelled mid-stream) => tokens None,
    // not 0. Session total also None (partial sum would undercount).
    let events = vec![
        ev(
            100,
            TurnEventKind::UserInput {
                text: "hello".into(),
            },
        ),
        ev(
            105,
            TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            TurnEventKind::AssistantMessage {
                text: "partial".into(),
                thinking: None,
            },
        ),
        // No TurnUsage — the turn was cancelled before Finish.
    ];
    let view = project(&events, "test");
    assert_eq!(view.total_turns, 1);
    let t = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(t.tokens_in, None, "unknown, not 0");
    assert_eq!(t.tokens_out, None);
    assert_eq!(
        view.tokens_in, None,
        "session total unknown when any turn is unknown"
    );
}

#[test]
fn test_project_empty_zero_view() {
    let view = project(&[], "test");
    assert_eq!(view.total_turns, 0);
    assert!(view.rows.is_empty());
}

#[test]
fn test_project_reasoning_carries_thinking() {
    // A Reasoning event projects to a "reasoning" event row carrying the
    // full thinking text (so the pane's L2 detail can show it without
    // re-scanning for sibling events); an AssistantMessage with a thinking
    // field carries it too. Pins the thinking projection — the cost of
    // surfacing reasoning is a trajectory dimension the record layer
    // spent a turn establishing.
    let events = vec![
        ev(
            100,
            TurnEventKind::UserInput {
                text: "explain".into(),
            },
        ),
        ev(
            105,
            TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            TurnEventKind::Reasoning {
                text: "let me think...".into(),
            },
        ),
        ev(
            120,
            TurnEventKind::AssistantMessage {
                text: "answer".into(),
                thinking: Some("let me think...".into()),
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let reasoning = turn
        .events
        .iter()
        .find(|e| e.kind == "reasoning")
        .expect("reasoning event projects to a row");
    assert_eq!(reasoning.thinking.as_deref(), Some("let me think..."));
    let llm = turn
        .events
        .iter()
        .find(|e| e.kind == "llm")
        .expect("assistant message projects to an llm row");
    assert_eq!(
        llm.thinking.as_deref(),
        Some("let me think..."),
        "the AssistantMessage carries its own thinking field"
    );
}

#[test]
fn test_cancelled_turn_omits_tokens() {
    let events = vec![
        ev(0, TurnEventKind::UserInput { text: "hi".into() }),
        ev(
            1,
            TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            2,
            TurnEventKind::AssistantMessage {
                text: "partial...".into(),
                thinking: None,
            },
        ),
        // No TurnUsage — the turn was cancelled before the provider returned
        // usage. Tokens must be None, not 0.
    ];
    let view = project(&events, "test");
    assert_eq!(view.total_turns, 1);
    let turn = view
        .rows
        .iter()
        .find_map(|r| match r {
            TrajectoryRow::Turn(t) => Some(t),
            _ => None,
        })
        .expect("one turn");
    assert!(turn.tokens_in.is_none(), "cancelled turn tokens_in None");
    assert!(turn.tokens_out.is_none(), "cancelled turn tokens_out None");
    assert!(turn.model.is_none(), "cancelled turn model None");
    assert!(turn.effort.is_none(), "cancelled turn effort None");
}

/// A failed bash tool result projects to a trajectory event whose L2 output
/// is the human-readable extract_body form (exit code + stderr), NOT the raw
/// JSON dump. Before the fix the trajectory pane showed
/// {"error":"...","exit_code":1,"stdout":"","stderr":"..."} on drill-down
/// while the transcript showed the formatted body — two renderings of the
/// same tool output. Now both route through extract_body.
#[test]
fn test_tool_result_extracts_body() {
    let events = vec![
        ev(100, TurnEventKind::UserInput { text: "go".into() }),
        ev(
            105,
            TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            TurnEventKind::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: serde_json::json!({"command": "false"}),
            },
        ),
        ev(
            120,
            TurnEventKind::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({
                    "stdout": "",
                    "stderr": "boom",
                    "exit_code": 1,
                    "success": false,
                }),
                duration_ms: 5,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let tr = turn
        .events
        .iter()
        .find(|e| e.kind == "tool_result")
        .expect("tool_result event");
    let body = tr.output.as_deref().unwrap_or("");
    assert!(
        body.contains("Exit code 1") && body.contains("boom"),
        "L2 output is the formatted body, not raw JSON: {body}"
    );
    assert!(
        !body.starts_with('{'),
        "L2 output must not be a raw JSON dump: {body}"
    );
    // The L1 summary previews the formatted body too (so the row reads
    // "Exit code 1", not "{\"stdout\":\"\",...}").
    assert!(
        tr.summary.contains("Exit code 1") || tr.summary.contains("boom"),
        "L1 summary previews the formatted body: {}",
        tr.summary
    );
}

/// A shell command that exits non-zero counts as a failure in the pane, the
/// same verdict the transcript chip reaches. A failing command reports itself
/// in exit_code and success and carries no error key, so an error-key-only
/// test called it a success: the pane showed a green row and a zero failure
/// total while the transcript painted the same command red.
#[test]
fn test_failed_bash_counted() {
    let events = vec![
        ev(100, TurnEventKind::UserInput { text: "go".into() }),
        ev(
            105,
            TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            TurnEventKind::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: serde_json::json!({"command": "false"}),
            },
        ),
        // The exact shape the bash tool emits on a failure: no error key.
        ev(
            120,
            TurnEventKind::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({
                    "stdout": "", "stderr": "", "exit_code": 1, "success": false,
                }),
                duration_ms: 5,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(view.failures, 1, "header failure total counts the failure");
    assert_eq!(turn.tool_fail, 1, "per-turn failure count");
    let tr = turn
        .events
        .iter()
        .find(|e| e.kind == "tool_result")
        .expect("tool_result event");
    assert!(!tr.success, "the result row is marked failed");
}

/// grep exiting 1 (no matches) is the command reporting a result, not
/// failing. The pane must agree with the transcript chip, which applies the
/// same semantic-exit exception — otherwise a search that found nothing
/// would inflate the session's failure total.
#[test]
fn test_grep_nomatch_ok() {
    let events = vec![
        ev(100, TurnEventKind::UserInput { text: "go".into() }),
        ev(
            105,
            TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            TurnEventKind::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: serde_json::json!({"command": "grep needle haystack.txt"}),
            },
        ),
        ev(
            120,
            TurnEventKind::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({
                    "stdout": "", "stderr": "", "exit_code": 1, "success": false,
                }),
                duration_ms: 5,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(view.failures, 0, "no matches is not a failure");
    assert_eq!(turn.tool_fail, 0, "per-turn count agrees");
    let tr = turn
        .events
        .iter()
        .find(|e| e.kind == "tool_result")
        .expect("tool_result event");
    assert!(tr.success, "the result row stays successful");
}

/// A tool-infrastructure failure (an error key, no exit code) is still a
/// failure — the exit-code rule must not replace the error-key rule.
#[test]
fn test_error_key_counted() {
    let events = vec![
        ev(100, TurnEventKind::UserInput { text: "go".into() }),
        ev(
            105,
            TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            TurnEventKind::ToolCall {
                call_id: "c1".into(),
                tool: "read".into(),
                input: serde_json::json!({"path": "/nope"}),
            },
        ),
        ev(
            120,
            TurnEventKind::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({"error": "permission denied"}),
                duration_ms: 1,
            },
        ),
    ];
    let view = project(&events, "test");
    assert_eq!(view.failures, 1);
}

/// A MetaUser event (system reminder — redundancy nudge, blind-retry warning)
/// must NOT enter the turn's user_input. The trajectory title reads
/// user_input; a system reminder showing there would mislead the user into
/// thinking they typed it. MetaUser is skipped in project_event (no
/// trajectory event row) and never sets user_input in the projection loop.
#[test]
fn test_meta_user_excluded() {
    let events = vec![
        ev(
            100,
            TurnEventKind::UserInput {
                text: "hello".into(),
            },
        ),
        ev(
            105,
            TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            TurnEventKind::MetaUser {
                text: "Note: you just called bash with the same input earlier".into(),
            },
        ),
        ev(
            120,
            TurnEventKind::TurnStarted {
                turn: 2,
                call_in_turn: 0,
            },
        ),
    ];
    let view = project(&events, "test");
    // Two turns: first (hello + MetaUser reminder), second (empty prompt
    // continuation).
    let first = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let second = match &view.rows[1] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    // First turn's user_input is the real prompt, not the MetaUser reminder.
    assert_eq!(first.user_input, "hello");
    // Second turn's user_input is empty (no UserInput between the first
    // turn's TurnStarted and the second's).
    assert!(
        second.user_input.is_empty(),
        "second turn user_input must be empty, got: {}",
        second.user_input
    );
    // The MetaUser reminder must NOT appear as a trajectory event in either
    // turn.
    for turn in [&first, &second] {
        for ev in &turn.events {
            assert!(
                !ev.summary.contains("Note: you just called"),
                "MetaUser leaked into trajectory events: {}",
                ev.summary
            );
        }
    }
}

/// A MemoryRecall event (system-reminder memories served to the model as
/// InputItem::User) must NOT enter the turn's user_input either. Same
/// class as MetaUser: system content the model sees as user, but the
/// trajectory must not display as user input.
#[test]
fn test_memory_recall_excluded() {
    let events = vec![
        ev(
            100,
            TurnEventKind::UserInput {
                text: "fix the bug".into(),
            },
        ),
        ev(
            105,
            TurnEventKind::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            TurnEventKind::MemoryRecall {
                text: "remembered: always run tests".into(),
                keys: vec![],
                bytes: 42,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        turn.user_input, "fix the bug",
        "MemoryRecall must not overwrite user_input"
    );
    for ev in &turn.events {
        assert!(
            !ev.summary.contains("remembered:"),
            "MemoryRecall leaked into trajectory events: {}",
            ev.summary
        );
    }
}
