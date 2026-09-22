//! Tests for trajectory view assembly: verifies turn aggregation from session
//! events, token and tool metrics, timing percentiles, and retry counts.

use super::*;
use houyicoder_context::{EventId, SessionLogEntry};

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
    let view = project(&events, "test");
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
    assert_eq!(t1.user_input, "hello");
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
    let tools: Vec<_> = t1
        .records
        .iter()
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
    let models: Vec<_> = t1
        .records
        .iter()
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
    assert_eq!(t1.records[0].kind, TrajectoryRecordKind::Context);
    assert_eq!(t1.records[0].start_ms, 0);
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
    let view = project(&events, "test");
    assert_eq!(
        view.total_turns, 1,
        "one prompt stays one turn however many model calls it drives"
    );
    let t1 = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(t1.user_input, "fix the bug");
    let models = t1
        .records
        .iter()
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
    // The retry burned real tokens, so the turn's cost is the sum of both
    // calls: a retry is not free and must not be hidden by taking the last.
    assert_eq!(t.tokens_in, Some(5000));
    assert_eq!(t.tokens_out, Some(1100));
    // The retry is attributed to the model call it belongs to.
    let retried: Vec<_> = t
        .records
        .iter()
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
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let reasoning = turn
        .records
        .iter()
        .find(|e| e.kind == TrajectoryRecordKind::Model)
        .expect("reasoning event projects to a row");
    assert_eq!(reasoning.thinking.as_deref(), Some("let me think..."));
    let llm = turn
        .records
        .iter()
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
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let tr = turn
        .records
        .iter()
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
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(view.failures, 1, "header failure total counts the failure");
    assert_eq!(turn.tool_fail, 1, "per-turn failure count");
    let tr = turn
        .records
        .iter()
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
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(view.failures, 0, "no matches is not a failure");
    assert_eq!(turn.tool_fail, 0, "per-turn count agrees");
    let tr = turn
        .records
        .iter()
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
    let view = project(&events, "test");
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
    let view = project(&events, "test");
    assert_eq!(
        view.total_turns, 1,
        "the second model call stays inside the one turn"
    );
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        turn.user_input, "hello",
        "the title is the real prompt, not the MetaUser reminder"
    );
    assert_eq!(
        turn.records
            .iter()
            .filter(|e| e.kind == TrajectoryRecordKind::Model)
            .count(),
        2,
        "both model calls are records in the turn"
    );
    for record in &turn.records {
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
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        turn.user_input, "fix the bug",
        "MemoryRecall must not overwrite user_input"
    );
    for ev in &turn.records {
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
    let view = project(&events, "qwen");
    assert_eq!(view.ttft_avg_ms, Some(400));
    assert_eq!(view.ttft_p95_ms, Some(600));
    assert_eq!(view.ttft_p99_ms, Some(600));
    assert_eq!(view.cache_read, Some(800));
    assert!(view.decode_tok_per_sec.is_some());
    let tps = view.decode_tok_per_sec.unwrap();
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
    let view = project(&events, "test");
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
        ev(150, SessionEvent::ContextCleared { prior_turn: 1 }),
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
    let view = project(&events, "test");
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
    assert_eq!(first.boundary_before, None);
    assert_eq!(
        second.boundary_before,
        Some(TurnBoundary::ContextCleared { prior_turn: 1 }),
        "the boundary attaches to the turn after the clear"
    );
    assert_eq!(second.n, 2, "numbering continues across the clear");
}

/// A tool call whose result never lands is pending, not a success: the pane
/// must not put a checkmark on work that may still fail.
#[test]
fn test_tool_without_result_pending() {
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
                input: serde_json::json!({"command": "sleep 999"}),
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let tool = turn
        .records
        .iter()
        .find(|r| r.kind == TrajectoryRecordKind::Tool)
        .expect("the call is a record");
    assert_eq!(tool.outcome, RecordOutcome::Pending);
    assert_eq!(tool.duration_ms, 0, "no result means no duration");
    assert!(tool.output.is_none());
    assert_eq!(turn.tool_count, 1);
    assert_eq!(turn.tool_fail, 0, "pending is not a failure either");
}

/// A delegation merges its spawn and return into one Agent record spanning the
/// child's life, so the parent turn shows the work it delegated rather than two
/// unrelated log lines.
#[test]
fn test_agent_merges_spawn_return() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "explore".into(),
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
            SessionEvent::SubagentSpawn {
                child_session_id: "child-1".into(),
                subagent_type: "explore".into(),
                prompt_summary: "find the auth code".into(),
                isolation: "worktree".into(),
                policy: "read-only".into(),
                trigger_source: "model:c1".into(),
            },
        ),
        ev(
            900,
            SessionEvent::SubagentReturn {
                child_session_id: "child-1".into(),
                status: "completed".into(),
                summary: "auth lives in src/auth.rs".into(),
                result_ref: "child-1".into(),
                input_tokens: 18000,
                output_tokens: 400,
                cache_read_input_tokens: 17000,
                cache_write_input_tokens: 0,
                reasoning_tokens: 100,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let agents: Vec<_> = turn
        .records
        .iter()
        .filter(|r| r.kind == TrajectoryRecordKind::Agent)
        .collect();
    assert_eq!(agents.len(), 1, "spawn and return are one record");
    assert_eq!(agents[0].name.as_deref(), Some("explore"));
    assert_eq!(agents[0].duration_ms, 790, "spans spawn to return");
    assert_eq!(agents[0].outcome, RecordOutcome::Ok);
    assert!(
        agents[0]
            .output
            .as_deref()
            .unwrap_or("")
            .contains("src/auth.rs"),
        "the child's result rides the same record"
    );
}

/// A queued user message delivered mid-turn is context inside the running turn,
/// not a new turn: the user did not start a new request.
#[test]
fn test_mid_turn_input_context() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "start".into(),
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
            SessionEvent::MidTurnInput {
                text: "also check the tests".into(),
                pending_input_id: None,
            },
        ),
    ];
    let view = project(&events, "test");
    assert_eq!(view.total_turns, 1, "an interjection is not a new turn");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let contexts: Vec<_> = turn
        .records
        .iter()
        .filter(|r| r.kind == TrajectoryRecordKind::Context)
        .collect();
    assert_eq!(contexts.len(), 2, "the prompt and the update");
    assert!(
        contexts[1]
            .input
            .as_deref()
            .unwrap_or("")
            .contains("also check the tests")
    );
}

/// A memory recall is its own record carrying the recalled keys, so the turn
/// shows what the model was handed rather than hiding it in the prompt.
#[test]
fn test_memory_recall_is_record() {
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
            SessionEvent::MemoryRecall {
                text: "remembered".into(),
                keys: vec!["commit-flow".into(), "gate-decide".into()],
                bytes: 2048,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let memory = turn
        .records
        .iter()
        .find(|r| r.kind == TrajectoryRecordKind::Memory)
        .expect("the recall is a record");
    assert!(
        memory.summary.contains("2 keys"),
        "the row counts the recalled keys: {}",
        memory.summary
    );
    assert!(memory.summary.contains("2.0KB"), "and their size");
    assert!(
        memory
            .output
            .as_deref()
            .unwrap_or("")
            .contains("commit-flow"),
        "the keys are available on drill-down"
    );
}

/// A model delegation is issued as a tool call. The turn shows one Agent record
/// for the delegation, so the tool call it was spawned from must not also
/// appear: that would count the same work twice, once as the mechanism and once
/// as the delegation.
#[test]
fn test_delegation_hides_tool_call() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "explore".into(),
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
            108,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "agent".into(),
                input: serde_json::json!({"type": "explore"}),
            },
        ),
        ev(
            110,
            SessionEvent::SubagentSpawn {
                child_session_id: "child-1".into(),
                subagent_type: "explore".into(),
                prompt_summary: "find the auth code".into(),
                isolation: "worktree".into(),
                policy: "read-only".into(),
                trigger_source: "model:c1".into(),
            },
        ),
        ev(
            900,
            SessionEvent::SubagentReturn {
                child_session_id: "child-1".into(),
                status: "completed".into(),
                summary: "done".into(),
                result_ref: "child-1".into(),
                input_tokens: 100,
                output_tokens: 10,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        turn.records
            .iter()
            .filter(|r| r.kind == TrajectoryRecordKind::Tool)
            .count(),
        0,
        "the delegation's tool call is not a separate record"
    );
    assert_eq!(
        turn.records
            .iter()
            .filter(|r| r.kind == TrajectoryRecordKind::Agent)
            .count(),
        1,
        "the delegation itself is"
    );
    assert_eq!(
        turn.tool_count, 0,
        "and it is not counted as a tool call either"
    );
}

/// A tool call the model made directly (not a delegation) still shows as a Tool
/// record, so the delegation rule does not swallow ordinary tool work.
#[test]
fn test_direct_tool_call_shows() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "read it".into(),
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
                call_id: "c9".into(),
                tool: "read".into(),
                input: serde_json::json!({"path": "a.rs"}),
            },
        ),
        ev(
            120,
            SessionEvent::ToolResult {
                call_id: "c9".into(),
                output: serde_json::json!({"content": "fn main() {}"}),
                duration_ms: 12,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let tools: Vec<_> = turn
        .records
        .iter()
        .filter(|r| r.kind == TrajectoryRecordKind::Tool)
        .collect();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name.as_deref(), Some("read"));
    assert_eq!(tools[0].outcome, RecordOutcome::Ok);
}

/// A hook verdict that is not a denial did not fail. Rendering an observation or
/// an injection as a red error would report a failure the hook never produced.
#[test]
fn test_hook_verdict_neutral() {
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
            SessionEvent::HookSignal {
                event: Default::default(),
                verdict: houyicoder_context::HookVerdictKind::Observe,
                error: None,
                reason: "noted".into(),
                hook_name: "audit".into(),
                tool_name: None,
                triggered_event: None,
                turn: None,
                call_in_turn: None,
            },
        ),
        ev(
            120,
            SessionEvent::HookSignal {
                event: Default::default(),
                verdict: houyicoder_context::HookVerdictKind::Deny,
                error: None,
                reason: "no backticks".into(),
                hook_name: "style".into(),
                tool_name: None,
                triggered_event: None,
                turn: None,
                call_in_turn: None,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let observed = turn
        .records
        .iter()
        .find(|r| r.name.as_deref() == Some("audit"))
        .expect("the observation is recorded");
    assert_eq!(
        observed.kind,
        TrajectoryRecordKind::Hook,
        "an observation is not an error"
    );
    assert_eq!(observed.outcome, RecordOutcome::Ok);
    let denied = turn
        .records
        .iter()
        .find(|r| r.name.as_deref() == Some("style"))
        .expect("the denial is recorded");
    assert_eq!(denied.kind, TrajectoryRecordKind::Error);
    assert_eq!(denied.outcome, RecordOutcome::Failed);
}

/// A spawn written before the trigger field existed carries an empty source,
/// which a replay reads as a model trigger. The delegation's own tool call must
/// still be suppressed, or the same work shows twice.
#[test]
fn test_delegation_legacy_hides_tool() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "explore".into(),
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
            108,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "agent".into(),
                input: serde_json::json!({"type": "explore"}),
            },
        ),
        ev(
            110,
            SessionEvent::SubagentSpawn {
                child_session_id: "child-1".into(),
                subagent_type: "explore".into(),
                prompt_summary: "find it".into(),
                isolation: "worktree".into(),
                policy: "read-only".into(),
                trigger_source: String::new(),
            },
        ),
        ev(
            900,
            SessionEvent::SubagentReturn {
                child_session_id: "child-1".into(),
                status: "completed".into(),
                summary: "done".into(),
                result_ref: "child-1".into(),
                input_tokens: 10,
                output_tokens: 1,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        turn.records
            .iter()
            .filter(|r| r.kind == TrajectoryRecordKind::Tool)
            .count(),
        0,
        "the delegation's tool call is suppressed by position"
    );
    assert_eq!(
        turn.records
            .iter()
            .filter(|r| r.kind == TrajectoryRecordKind::Agent)
            .count(),
        1
    );
}

/// A return whose spawn sits outside the loaded window is still shown, rather
/// than dropped without trace.
#[test]
fn test_agent_return_shown() {
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
            900,
            SessionEvent::SubagentReturn {
                child_session_id: "child-9".into(),
                status: "completed".into(),
                summary: "done".into(),
                result_ref: "child-9".into(),
                input_tokens: 10,
                output_tokens: 1,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let agents: Vec<_> = turn
        .records
        .iter()
        .filter(|r| r.kind == TrajectoryRecordKind::Agent)
        .collect();
    assert_eq!(agents.len(), 1, "the return is not dropped");
    assert!(agents[0].summary.contains("child-9"));
}

/// An unrecognised delegation status is unknown, not a success.
#[test]
fn test_agent_unknown_status() {
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
            SessionEvent::SubagentSpawn {
                child_session_id: "child-1".into(),
                subagent_type: "explore".into(),
                prompt_summary: "p".into(),
                isolation: "worktree".into(),
                policy: "read-only".into(),
                trigger_source: "model:c1".into(),
            },
        ),
        ev(
            900,
            SessionEvent::SubagentReturn {
                child_session_id: "child-1".into(),
                status: "timeout".into(),
                summary: String::new(),
                result_ref: "child-1".into(),
                input_tokens: 10,
                output_tokens: 1,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let agent = turn
        .records
        .iter()
        .find(|r| r.kind == TrajectoryRecordKind::Agent)
        .expect("the delegation is a record");
    assert_eq!(
        agent.outcome,
        RecordOutcome::Failed,
        "a timeout is a failure"
    );
}

/// Model calls are numbered inside their turn, so a multi-call turn reads as a
/// sequence rather than as interchangeable rows.
#[test]
fn test_model_calls_are_numbered() {
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
            SessionEvent::AssistantMessage {
                text: "a".into(),
                thinking: None,
            },
        ),
        ev(
            200,
            SessionEvent::TurnStarted {
                turn: 2,
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
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let ordinals: Vec<u32> = turn
        .records
        .iter()
        .filter(|r| r.kind == TrajectoryRecordKind::Model)
        .map(|r| r.ordinal)
        .collect();
    assert_eq!(ordinals, vec![1, 2], "the calls are numbered in order");
}

/// A call that produced a reply is complete even when no timing was recorded,
/// so it does not stay pending forever.
#[test]
fn test_reply_completes_model_call() {
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
            SessionEvent::AssistantMessage {
                text: "answer".into(),
                thinking: None,
            },
        ),
    ];
    let view = project(&events, "test");
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    let model = turn
        .records
        .iter()
        .find(|r| r.kind == TrajectoryRecordKind::Model)
        .expect("the call is a record");
    assert_eq!(model.outcome, RecordOutcome::Ok);
}

/// A turn opened by a non-boundary event still gets a number, so a windowed read
/// that starts mid-run cannot label it turn zero.
#[test]
fn test_turn_opened_by_content() {
    let events = vec![ev(
        100,
        SessionEvent::AssistantMessage {
            text: "mid-run".into(),
            thinking: None,
        },
    )];
    let view = project(&events, "test");
    assert_eq!(view.total_turns, 1);
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(turn.n, 1, "a turn is never numbered zero");
}
