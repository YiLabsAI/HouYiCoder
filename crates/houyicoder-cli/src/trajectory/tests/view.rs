//! Tests for the turn fold: grouping, per-turn totals, records, and the
//! timing and boundary facts it derives from the durable log.

use super::super::turns::FoldMode;
use super::super::view::*;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_tui::view::trajectory_pane::{
    CompactedBoundary, ModelSwitchBoundary, RecordOutcome, TrajectoryRecordKind, TrajectoryRow,
    TrajectoryTurn, TurnBoundary,
};

fn ev(ts: u64, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts,
        prev_hash: None,
        event: kind,
    }
}

#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
#[test]
fn test_turn_groups_by_input() {
    // One prompt that needs two model calls (a tool round trip between them)
    // is ONE turn: the user asked once. The second TurnStarted is the second
    // model call inside that turn, not a second turn.
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "hello".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
        ),
        ev(
            120,
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({"echo": 1}),
                duration_ms: 50,
            },
        ),
        ev(
            130,
            SessionEvent::TurnUsage {
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
            SessionEvent::AssistantMessage {
                text: "hi".into(),
                thinking: None,
            },
        ),
        ev(
            210,
            SessionEvent::TurnStarted {
                turn: 2,
                call_in_turn: 0,
            },
        ),
        ev(
            220,
            SessionEvent::ToolCall {
                call_id: "c2".into(),
                tool: "echo".into(),
                input: serde_json::json!({}),
            },
        ),
        ev(
            230,
            SessionEvent::ToolResult {
                call_id: "c2".into(),
                output: serde_json::json!({"error": "boom"}),
                duration_ms: 10,
            },
        ),
        ev(
            240,
            SessionEvent::TurnUsage {
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
    let view = project(&events, "test", 0);
    assert_eq!(
        view.total_turns, 1,
        "one prompt is one turn however many model calls it takes"
    );
    assert_eq!(view.tokens_in, Some(3000), "session total sums both calls");
    assert_eq!(view.tokens_out, Some(600));
    assert_eq!(view.failures, 1);
    let t1 = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(t1.n, 1);
    assert_eq!(t1.title, "hello");
    assert_eq!(
        t1.tokens_in,
        Some(3000),
        "a turn sums every model call it made, not just the last"
    );
    assert_eq!(t1.tool_count, 2);
    assert_eq!(t1.tool_fail, 1);
    assert_eq!(t1.retries, 0);
    assert_eq!(
        t1.duration_ms, 140,
        "the turn spans its own events (100 to 240), not just its tools"
    );
    // Each tool call is one record carrying both its input and its result.
    let tools: Vec<_> = records_of(&events, t1.n)
        .into_iter()
        .filter(|e| e.kind == TrajectoryRecordKind::Tool)
        .collect();
    assert_eq!(tools.len(), 2, "call and result merge into one record");
    assert_eq!(tools[0].name.as_deref(), Some("echo"));
    assert!(tools[0].input.is_some(), "the call input is on the record");
    assert!(
        tools[0].output.is_some(),
        "the result is on the same record"
    );
    assert_eq!(tools[0].duration_ms, 50);
    assert_eq!(tools[0].outcome, RecordOutcome::Ok);
    assert_eq!(
        tools[1].outcome,
        RecordOutcome::Failed,
        "the error result marks its record failed"
    );
    // Two model calls inside the turn, each with its own usage.
    let models: Vec<_> = records_of(&events, t1.n)
        .into_iter()
        .filter(|e| e.kind == TrajectoryRecordKind::Model)
        .collect();
    assert_eq!(models.len(), 2, "each model call is its own record");
    assert_eq!(
        models[0].usage.and_then(|u| u.input),
        Some(1000),
        "a model call keeps its own usage for L2 attribution"
    );
    assert_eq!(models[1].usage.and_then(|u| u.input), Some(2000));
    // The user input is the turn's first record, offset zero.
    assert_eq!(
        records_of(&events, t1.n)[0].kind,
        TrajectoryRecordKind::Context
    );
    assert_eq!(records_of(&events, t1.n)[0].start_ms, 0);
}

#[test]
fn test_multi_iteration_produces_turns() {
    // One prompt, three model calls (two tool round trips between them).
    // The calls must stay visible as records inside the single turn.
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "fix the bug".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            120,
            SessionEvent::TurnUsage {
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
            SessionEvent::TurnStarted {
                turn: 2,
                call_in_turn: 0,
            },
        ),
        ev(
            320,
            SessionEvent::TurnUsage {
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
            SessionEvent::TurnStarted {
                turn: 3,
                call_in_turn: 0,
            },
        ),
        ev(
            520,
            SessionEvent::TurnUsage {
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
    let view = project(&events, "test", 0);
    assert_eq!(
        view.total_turns, 1,
        "one prompt stays one turn however many model calls it drives"
    );
    let t1 = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(t1.title, "fix the bug");
    let models = records_of(&events, t1.n)
        .into_iter()
        .filter(|e| e.kind == TrajectoryRecordKind::Model)
        .count();
    assert_eq!(models, 3, "the three calls are visible inside the turn");
    assert_eq!(
        t1.tokens_in,
        Some(16200),
        "the turn's cost is the sum of its calls"
    );
    assert_eq!(t1.tokens_out, Some(2300));
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
            SessionEvent::UserInput {
                text: "long reply".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::AssistantMessage {
                text: "partial".into(),
                thinking: None,
            },
        ),
        ev(
            120,
            SessionEvent::TurnUsage {
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
            SessionEvent::AssistantMessage {
                text: " done".into(),
                thinking: None,
            },
        ),
        ev(
            140,
            SessionEvent::TurnUsage {
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
    let view = project(&events, "test", 0);
    assert_eq!(
        view.total_turns, 1,
        "retry stays on the same turn, not a new turn"
    );
    let t = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(t.retries, 1, "one recovery=true TurnUsage => retries 1");
    // The retry burned real tokens, so the turn's cost is the sum of both
    // calls: a retry is not free and must not be hidden by taking the last.
    assert_eq!(t.tokens_in, Some(5000));
    assert_eq!(t.tokens_out, Some(1100));
    // The retry is attributed to the model call it belongs to.
    let retried: Vec<_> = records_of(&events, t.n)
        .into_iter()
        .filter(|e| e.kind == TrajectoryRecordKind::Model && e.retries > 0)
        .collect();
    assert_eq!(retried.len(), 1, "the recovery call carries the retry");
}

#[test]
fn test_tokens_none_no_usage() {
    // A turn with no TurnUsage (cancelled mid-stream) => tokens None,
    // not 0. Session total also None (partial sum would undercount).
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "hello".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::AssistantMessage {
                text: "partial".into(),
                thinking: None,
            },
        ),
        // No TurnUsage — the turn was cancelled before Finish.
    ];
    let view = project(&events, "test", 0);
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
    let view = project(&[], "test", 0);
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
            SessionEvent::UserInput {
                text: "explain".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::Reasoning {
                text: "let me think...".into(),
            },
        ),
        ev(
            120,
            SessionEvent::AssistantMessage {
                text: "answer".into(),
                thinking: Some("let me think...".into()),
            },
        ),
    ];
    let view = project(&events, "test", 0);
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let reasoning = records_of(&events, turn.n)
        .into_iter()
        .find(|e| e.kind == TrajectoryRecordKind::Model)
        .expect("reasoning event projects to a row");
    assert_eq!(reasoning.thinking.as_deref(), Some("let me think..."));
    let llm = records_of(&events, turn.n)
        .into_iter()
        .find(|e| e.kind == TrajectoryRecordKind::Model)
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
        ev(0, SessionEvent::UserInput { text: "hi".into() }),
        ev(
            1,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            2,
            SessionEvent::AssistantMessage {
                text: "partial...".into(),
                thinking: None,
            },
        ),
        // No TurnUsage — the turn was cancelled before the provider returned
        // usage. Tokens must be None, not 0.
    ];
    let view = project(&events, "test", 0);
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
    assert!(turn.models.is_empty(), "cancelled turn model None");
    assert!(turn.efforts.is_empty(), "cancelled turn effort None");
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
        ev(100, SessionEvent::UserInput { text: "go".into() }),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: serde_json::json!({"command": "false"}),
            },
        ),
        ev(
            120,
            SessionEvent::ToolResult {
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
    let view = project(&events, "test", 0);
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let tr = records_of(&events, turn.n)
        .into_iter()
        .find(|e| e.kind == TrajectoryRecordKind::Tool)
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
        ev(100, SessionEvent::UserInput { text: "go".into() }),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: serde_json::json!({"command": "false"}),
            },
        ),
        // The exact shape the bash tool emits on a failure: no error key.
        ev(
            120,
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({
                    "stdout": "", "stderr": "", "exit_code": 1, "success": false,
                }),
                duration_ms: 5,
            },
        ),
    ];
    let view = project(&events, "test", 0);
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(view.failures, 1, "header failure total counts the failure");
    assert_eq!(turn.tool_fail, 1, "per-turn failure count");
    let tr = records_of(&events, turn.n)
        .into_iter()
        .find(|e| e.kind == TrajectoryRecordKind::Tool)
        .expect("tool_result event");
    assert_eq!(
        tr.outcome,
        RecordOutcome::Failed,
        "the result row is marked failed"
    );
}

/// grep exiting 1 (no matches) is the command reporting a result, not
/// failing. The pane must agree with the transcript chip, which applies the
/// same semantic-exit exception — otherwise a search that found nothing
/// would inflate the session's failure total.
#[test]
fn test_grep_nomatch_ok() {
    let events = vec![
        ev(100, SessionEvent::UserInput { text: "go".into() }),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: serde_json::json!({"command": "grep needle haystack.txt"}),
            },
        ),
        ev(
            120,
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({
                    "stdout": "", "stderr": "", "exit_code": 1, "success": false,
                }),
                duration_ms: 5,
            },
        ),
    ];
    let view = project(&events, "test", 0);
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(view.failures, 0, "no matches is not a failure");
    assert_eq!(turn.tool_fail, 0, "per-turn count agrees");
    let tr = records_of(&events, turn.n)
        .into_iter()
        .find(|e| e.kind == TrajectoryRecordKind::Tool)
        .expect("tool_result event");
    assert_eq!(
        tr.outcome,
        RecordOutcome::Ok,
        "the result row stays successful"
    );
}

/// A tool-infrastructure failure (an error key, no exit code) is still a
/// failure — the exit-code rule must not replace the error-key rule.
#[test]
fn test_error_key_counted() {
    let events = vec![
        ev(100, SessionEvent::UserInput { text: "go".into() }),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "read".into(),
                input: serde_json::json!({"path": "/nope"}),
            },
        ),
        ev(
            120,
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({"error": "permission denied"}),
                duration_ms: 1,
            },
        ),
    ];
    let view = project(&events, "test", 0);
    assert_eq!(view.failures, 1);
}

/// A MetaUser event (system reminder — redundancy nudge, blind-retry warning)
/// must NOT enter the turn's user_input. The trajectory title reads
/// user_input; a system reminder showing there would mislead the user into
/// thinking they typed it. MetaUser is skipped in build_record (no
/// trajectory event row) and never sets user_input in the projection loop.
#[test]
fn test_meta_user_excluded() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "hello".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::MetaUser {
                text: "Note: you just called bash with the same input earlier".into(),
            },
        ),
        ev(
            120,
            SessionEvent::TurnStarted {
                turn: 2,
                call_in_turn: 0,
            },
        ),
    ];
    let view = project(&events, "test", 0);
    assert_eq!(
        view.total_turns, 1,
        "the second model call stays inside the one turn"
    );
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        turn.title, "hello",
        "the title is the real prompt, not the MetaUser reminder"
    );
    assert_eq!(
        records_of(&events, turn.n)
            .iter()
            .filter(|e| e.kind == TrajectoryRecordKind::Model)
            .count(),
        2,
        "both model calls are records in the turn"
    );
    for record in &records_of(&events, turn.n) {
        assert!(
            !record.summary.contains("Note: you just called"),
            "MetaUser leaked into trajectory records: {}",
            record.summary
        );
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
            SessionEvent::UserInput {
                text: "fix the bug".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::MemoryRecall {
                text: "remembered: always run tests".into(),
                keys: vec![],
                bytes: 42,
            },
        ),
    ];
    let view = project(&events, "test", 0);
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        turn.title, "fix the bug",
        "MemoryRecall must not overwrite user_input"
    );
    for ev in &records_of(&events, turn.n) {
        assert!(
            !ev.summary.contains("remembered:"),
            "MemoryRecall leaked into trajectory events: {}",
            ev.summary
        );
    }
}

/// Verify that ModelStepTiming events compute correct TTFT percentiles and decode speed.
#[test]
fn test_timing_percentiles_and_speed() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "calc".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::TurnUsage {
                turn: 1,
                call_in_turn: 0,
                input_tokens: 1000,
                output_tokens: 250,
                cache_read_input_tokens: 800,
                cache_write_input_tokens: 0,
                reasoning_tokens: 50,
                model: "qwen".into(),
                effort: None,
                recovery: false,
            },
        ),
        ev(
            115,
            SessionEvent::ModelStepTiming {
                turn: 1,
                step: 0,
                total_ms: 1000,
                ttft_ms: Some(200),
                decode_ms: Some(800),
            },
        ),
        ev(
            120,
            SessionEvent::ModelStepTiming {
                turn: 1,
                step: 1,
                total_ms: 1500,
                ttft_ms: Some(600),
                decode_ms: Some(900),
            },
        ),
    ];
    let view = project(&events, "qwen", 0);
    assert_eq!(view.timing.ttft_avg_ms, Some(400));
    assert_eq!(view.timing.ttft_p95_ms, Some(600));
    assert_eq!(view.timing.ttft_p99_ms, Some(600));
    assert_eq!(view.cache_read, Some(800));
    assert!(view.timing.decode_tok_per_sec.is_some());
    let tps = view.timing.decode_tok_per_sec.unwrap();
    assert!((tps - (250.0 / 1.7)).abs() < 0.1);
}

/// Turn numbering survives a runner rebuild. The runner's TurnStarted counter
/// lives in the process and restarts at 1 whenever the runner is rebuilt
/// (resume, reconnect), but the session log keeps growing. Numbering turns from
/// the user inputs in the log keeps the sequence monotonic; using the counter
/// would repeat the first turn number in the middle of one session.
#[test]
fn test_turn_ids_survive_rebuild() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "first".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::AssistantMessage {
                text: "a".into(),
                thinking: None,
            },
        ),
        // Runner rebuilt: the counter restarts, the log does not.
        ev(
            200,
            SessionEvent::UserInput {
                text: "second".into(),
            },
        ),
        ev(
            205,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            210,
            SessionEvent::AssistantMessage {
                text: "b".into(),
                thinking: None,
            },
        ),
    ];
    let view = project(&events, "test", 0);
    assert_eq!(view.total_turns, 2);
    let n: Vec<usize> = view
        .rows
        .iter()
        .filter_map(|r| match r {
            TrajectoryRow::Turn(t) => Some(t.n),
            _ => None,
        })
        .collect();
    assert_eq!(n, vec![1, 2], "the repeated counter must not repeat the id");
}

/// Clearing the context records a boundary on the next turn and does not
/// restart turn numbering: the user cleared the conversation, not the session.
#[test]
fn test_context_cleared_records_boundary() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "first".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            1_700_000_000_000,
            SessionEvent::ContextCleared { prior_turn: 1 },
        ),
        ev(
            200,
            SessionEvent::UserInput {
                text: "after clear".into(),
            },
        ),
        ev(
            205,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
    ];
    let view = project(&events, "test", 0);
    assert_eq!(
        view.total_turns, 2,
        "a clear does not create or drop a turn"
    );
    let first = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let second = match &view.rows[1] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert!(first.boundary_before.is_empty());
    assert_eq!(
        second.boundary_before,
        vec![TurnBoundary::ContextCleared {
            prior_turn: 1,
            at_secs: 1_700_000_000,
        }],
        "the boundary attaches to the turn after the clear"
    );
    assert_eq!(second.n, 2, "numbering continues across the clear");
}

/// A model switch between turns records a boundary so the latency and cache
/// shift across models is visible in the timeline.
#[test]
fn test_model_switch_boundary() {
    let events = vec![
        ev(100, SessionEvent::UserInput { text: "t1".into() }),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            120,
            SessionEvent::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 100,
                output_tokens: 10,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "qwen".into(),
                recovery: false,
                effort: None,
            },
        ),
        ev(200, SessionEvent::UserInput { text: "t2".into() }),
        ev(
            205,
            SessionEvent::TurnStarted {
                turn: 2,
                call_in_turn: 0,
            },
        ),
        ev(
            220,
            SessionEvent::TurnUsage {
                turn: 2,
                call_in_turn: 1,
                input_tokens: 100,
                output_tokens: 10,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "deepseek".into(),
                recovery: false,
                effort: None,
            },
        ),
    ];
    let view = project(&events, "qwen", 0);
    assert_eq!(view.total_turns, 2);
    let second = match &view.rows[1] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        second.boundary_before,
        vec![TurnBoundary::ModelSwitch(Box::new(ModelSwitchBoundary {
            from: "qwen".into(),
            to: "deepseek".into(),
            at_secs: 0,
        }))]
    );
}

/// A compaction between turns records a boundary naming the checkpoint.
#[test]
fn test_compaction_boundary_attached() {
    use houyicoder_context::CheckpointId;
    let ck = CheckpointId::new();
    let events = vec![
        ev(100, SessionEvent::UserInput { text: "t1".into() }),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            150,
            SessionEvent::CompactionBoundary {
                checkpoint: ck,
                pre_tokens: 0,
                post_tokens: 0,
            },
        ),
        ev(200, SessionEvent::UserInput { text: "t2".into() }),
        ev(
            205,
            SessionEvent::TurnStarted {
                turn: 2,
                call_in_turn: 0,
            },
        ),
    ];
    let view = project(&events, "test", 0);
    assert_eq!(view.total_turns, 2);
    let second = match &view.rows[1] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        second.boundary_before,
        vec![TurnBoundary::Compacted(Box::new(CompactedBoundary {
            checkpoint_id: ck.to_string(),
            pre_tokens: 0,
            post_tokens: 0,
            at_secs: 0,
        }))]
    );
}

/// Two durable facts in the gap between turns both survive: a compaction
/// followed by a model switch is two boundaries, not one. A single slot would
/// have dropped one of them without saying so.
#[test]
fn test_two_boundaries_kept() {
    use houyicoder_context::CheckpointId;
    let ck = CheckpointId::new();
    let usage = |ts: u64, model: &str| {
        ev(
            ts,
            SessionEvent::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 100,
                output_tokens: 10,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: model.into(),
                recovery: false,
                effort: None,
            },
        )
    };
    let events = vec![
        ev(100, SessionEvent::UserInput { text: "t1".into() }),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        usage(110, "qwen"),
        ev(
            150,
            SessionEvent::CompactionBoundary {
                checkpoint: ck,
                pre_tokens: 0,
                post_tokens: 0,
            },
        ),
        ev(200, SessionEvent::UserInput { text: "t2".into() }),
        ev(
            205,
            SessionEvent::TurnStarted {
                turn: 2,
                call_in_turn: 0,
            },
        ),
        usage(210, "deepseek"),
    ];
    let view = project(&events, "test", 0);
    let second = match &view.rows[1] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        second.boundary_before.len(),
        2,
        "both durable facts are kept, in the order they happened; got {:?}",
        second.boundary_before
    );
    assert!(matches!(
        second.boundary_before[0],
        TurnBoundary::Compacted(_)
    ));
    assert!(matches!(
        second.boundary_before[1],
        TurnBoundary::ModelSwitch(_)
    ));
}

/// A model switch inside an open turn is not a list boundary: the turn's own
/// model records already show it, and a separator above the turn would claim
/// the switch happened between turns.
#[test]
fn test_switch_inside_turn() {
    let usage = |ts: u64, model: &str| {
        ev(
            ts,
            SessionEvent::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 100,
                output_tokens: 10,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: model.into(),
                recovery: false,
                effort: None,
            },
        )
    };
    let events = vec![
        ev(100, SessionEvent::UserInput { text: "t1".into() }),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        usage(110, "qwen"),
        usage(160, "deepseek"),
        ev(200, SessionEvent::UserInput { text: "t2".into() }),
        ev(
            205,
            SessionEvent::TurnStarted {
                turn: 2,
                call_in_turn: 0,
            },
        ),
    ];
    let view = project(&events, "test", 0);
    let first = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        first.models,
        vec!["qwen".to_string(), "deepseek".to_string()],
        "the turn records both models it used"
    );
    let second = match &view.rows[1] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert!(
        second.boundary_before.is_empty(),
        "a switch inside the previous turn is not a boundary above this one"
    );
}

/// A turn names itself by the event that opened it, so a caller can ask for
/// that turn's detail later without holding a row index or a turn number.
#[test]
fn test_turn_key_opening_event() {
    let first = ev(
        100,
        SessionEvent::UserInput {
            text: "hello".into(),
        },
    );
    let second = ev(
        200,
        SessionEvent::UserInput {
            text: "again".into(),
        },
    );
    let first_id = first.id.to_string();
    let second_id = second.id.to_string();
    let view = project(&[first, second], "m", 0);

    let keys: Vec<&str> = view
        .rows
        .iter()
        .filter_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.key.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        vec![first_id.as_str(), second_id.as_str()],
        "each turn carries the id of the event that opened it"
    );
}

/// A log that opens mid-run has no user input to name its first turn, so that
/// turn takes the id of the event that did open it.
#[test]
fn test_turn_key_without_input() {
    let opening = ev(
        100,
        SessionEvent::TurnStarted {
            turn: 1,
            call_in_turn: 0,
        },
    );
    let opening_id = opening.id.to_string();
    let view = project(&[opening], "m", 0);
    match view.rows.first() {
        Some(TrajectoryRow::Turn(turn)) => assert_eq!(
            turn.key.as_str(),
            opening_id,
            "a turn with no user input still names itself"
        ),
        _ => panic!("a leading turn row"),
    }
}

/// A row is titled by its prompt, by the first record's summary when the prompt
/// is not in the loaded window, and by a placeholder when there is neither — a
/// row is never blank. The title is derived where the records are, so the list
/// names a row without asking for a turn's detail.
#[test]
fn test_turn_title_derivation() {
    let with_input = ev(
        100,
        SessionEvent::UserInput {
            text: "fix the bug".into(),
        },
    );
    let view = project(&[with_input], "m", 0);
    match &view.rows[0] {
        TrajectoryRow::Turn(turn) => assert_eq!(turn.title, "fix the bug"),
        _ => panic!("a turn row"),
    }

    // A turn whose prompt sits outside the window: the first record names it.
    let mid_run = ev(
        100,
        SessionEvent::TurnStarted {
            turn: 1,
            call_in_turn: 0,
        },
    );
    let call = ev(
        110,
        SessionEvent::ToolCall {
            call_id: "c1".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "ls -la"}),
        },
    );
    let view = project(&[mid_run.clone(), call.clone()], "m", 0);
    match &view.rows[0] {
        TrajectoryRow::Turn(turn) => {
            // The rule the list used to apply: the prompt, or the first
            // record's summary when that is not blank, or the placeholder.
            let expected = records_of(&[mid_run.clone(), call.clone()], turn.n)
                .first()
                .map(|record| record.summary.clone())
                .filter(|summary| !summary.trim().is_empty())
                .unwrap_or_else(|| "(no input)".to_string());
            assert_eq!(turn.title, expected, "the row is named by its first record");
        }
        _ => panic!("a turn row"),
    }

    // Nothing to derive from: the placeholder, never a blank row.
    let only_boundary = ev(100, SessionEvent::ContextCleared { prior_turn: 1 });
    let view = project(&[only_boundary], "m", 0);
    assert!(
        view.rows.is_empty()
            || matches!(&view.rows[0], TrajectoryRow::Turn(t) if !t.title.trim().is_empty()),
        "a row is never blank"
    );
}

/// The list's fold and a drill's fold agree about every turn: what a row shows
/// must not depend on which of the two built it. The two differ only in whether
/// a record keeps its thinking, input, and output.
#[test]
fn test_fold_modes_agree() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "fix the bug".into(),
            },
        ),
        ev(
            110,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
        ),
        ev(
            120,
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({"text": "a b c"}),
                duration_ms: 7,
            },
        ),
        ev(
            130,
            SessionEvent::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 10,
                output_tokens: 4,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 0,
                reasoning_tokens: 1,
                model: "m".into(),
                recovery: false,
                effort: None,
            },
        ),
        ev(
            140,
            SessionEvent::UserInput {
                text: "and again".into(),
            },
        ),
        ev(
            150,
            SessionEvent::TurnAborted {
                reason: "user".into(),
            },
        ),
    ];

    let (summary, _) = fold_rows(&events, 1, FoldMode::Summary);
    let (records, _) = fold_rows(&events, 1, FoldMode::Records);
    let turns = |rows: &[TrajectoryRow]| -> Vec<TrajectoryTurn> {
        rows.iter()
            .filter_map(|row| match row {
                TrajectoryRow::Turn(turn) => Some(turn.clone()),
                TrajectoryRow::Bg(_) => None,
            })
            .collect()
    };
    let summaries = turns(&summary);
    let detailed = turns(&records);

    assert_eq!(summaries.len(), 2, "two turns");
    assert_eq!(
        summaries, detailed,
        "the two folds describe the same turns, field for field"
    );
}

/// A delegation's trigger call is drawn as the delegation even when a batch
/// boundary falls between the call and the spawn that claims it: the same work
/// must not appear twice, once as the mechanism and once as the delegation.
#[test]
fn test_delegation_split_across_batches() {
    let input = ev(
        100,
        SessionEvent::UserInput {
            text: "delegate".into(),
        },
    );
    let call = ev(
        110,
        SessionEvent::ToolCall {
            call_id: "c1".into(),
            tool: "agent".into(),
            input: serde_json::json!({"prompt": "go"}),
        },
    );
    let spawn = ev(
        120,
        SessionEvent::SubagentSpawn {
            child_session_id: "child".into(),
            subagent_type: "worker".into(),
            prompt_summary: "go".into(),
            isolation: String::new(),
            policy: String::new(),
            trigger_source: "model:c1".into(),
        },
    );
    let result = ev(
        130,
        SessionEvent::ToolResult {
            call_id: "c1".into(),
            output: serde_json::json!({"text": "done"}),
            duration_ms: 5,
        },
    );

    // The call lands in one batch; the spawn that claims it lands in the next.
    let mut split = PageProjection::seed(&[input.clone(), call.clone()], 1, FoldMode::Summary);
    split.apply(&[spawn.clone(), result.clone()]);
    let split_rows = split.finish();

    // A fold that saw the whole turn at once must agree with it.
    let (whole_rows, _) = fold_rows(&[input, call, spawn, result], 1, FoldMode::Summary);

    let counts = |rows: &[TrajectoryRow]| -> Vec<(usize, usize)> {
        rows.iter()
            .filter_map(|row| match row {
                TrajectoryRow::Turn(turn) => Some((turn.tool_count, turn.tool_fail)),
                TrajectoryRow::Bg(_) => None,
            })
            .collect()
    };
    assert_eq!(
        counts(&split_rows),
        counts(&whole_rows),
        "a split batch counts the delegation the way a whole fold does"
    );
    assert_eq!(
        counts(&split_rows),
        vec![(0, 0)],
        "the delegation's tool call is not counted as a plain tool call"
    );
}
