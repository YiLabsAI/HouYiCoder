//! Tests for per-record assembly: tool merging, delegations, context and
//! memory rows, hook verdicts, and call numbering.

use super::super::view::*;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_tui::view::trajectory_pane::{RecordOutcome, TrajectoryRecordKind, TrajectoryRow};

fn ev(ts: u64, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts,
        prev_hash: None,
        event: kind,
    }
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
    let view = project(&events, "test", 0);
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
    let view = project(&events, "test", 0);
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
    let usage = agents[0]
        .usage
        .expect("child usage is attached to the record for L2 display");
    assert_eq!(usage.input, Some(18000));
    assert_eq!(usage.output, Some(400));
    assert_eq!(usage.cache_read, Some(17000));
    assert_eq!(
        turn.tokens_in,
        Some(18000),
        "turn row tokens fold the child usage too"
    );
    assert_eq!(turn.tokens_out, Some(400));
    assert_eq!(turn.cache_read, Some(17000));
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
    let view = project(&events, "test", 0);
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
    assert_eq!(
        contexts[1].name.as_deref(),
        Some("User update"),
        "the update is labelled on the context lane"
    );
    assert!(
        contexts[1].summary.contains("User update:"),
        "the summary names the interjection: {}",
        contexts[1].summary
    );
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
    let view = project(&events, "test", 0);
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
        memory.summary.contains("Recall 2 items"),
        "the row names the recall and counts its items: {}",
        memory.summary
    );
    assert!(
        memory.input.as_deref().unwrap_or("").contains("2.0KB"),
        "the injected size is on the drill-down: {:?}",
        memory.input
    );
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
    let view = project(&events, "test", 0);
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
    let view = project(&events, "test", 0);
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
    let view = project(&events, "test", 0);
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
    let view = project(&events, "test", 0);
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
    let view = project(&events, "test", 0);
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
    let view = project(&events, "test", 0);
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
    let view = project(&events, "test", 0);
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
    let view = project(&events, "test", 0);
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
    let view = project(&events, "test", 0);
    assert_eq!(view.total_turns, 1);
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(turn.n, 1, "a turn is never numbered zero");
}
