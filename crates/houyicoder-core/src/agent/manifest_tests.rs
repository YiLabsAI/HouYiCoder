use super::*;
use crate::agent::context::Tokenizer;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};

fn ev(session: SessionId, id: EventId, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id,
        session,
        ts: 0,
        prev_hash: None,
        event: kind,
    }
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserInput { text: text.into() }
}

fn assistant(text: &str) -> SessionEvent {
    SessionEvent::AssistantMessage {
        text: text.into(),
        thinking: None,
    }
}

fn call(call_id: &str, tool: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        call_id: call_id.into(),
        tool: tool.into(),
        input: serde_json::json!({}),
    }
}

fn result(call_id: &str, output: serde_json::Value) -> SessionEvent {
    SessionEvent::ToolResult {
        call_id: call_id.into(),
        output,
        duration_ms: 0,
    }
}

fn ids(n: usize) -> Vec<EventId> {
    (0..n).map(|_| EventId::new()).collect()
}

fn disposition_of(manifest: &CheckpointManifest, id: EventId) -> Disposition {
    manifest
        .plan
        .iter()
        .find(|g| g.event_ids.contains(&id))
        .map(|g| g.disposition)
        .expect("event id present in plan")
}

/// Find the group index holding an event, to assert thinking and its
/// tool_use share a group (the turn-atomic property build_manifest
/// guarantees by construction).
fn group_of(manifest: &CheckpointManifest, id: EventId) -> usize {
    manifest
        .plan
        .iter()
        .position(|g| g.event_ids.contains(&id))
        .expect("event id present in plan")
}

#[tokio::test]
async fn test_tail_summarizes_older() {
    let s = SessionId::new();
    let ids = ids(6);
    let events = vec![
        ev(s, ids[0], user("do the task")),
        ev(s, ids[1], call("c0", "run")),
        ev(s, ids[2], result("c0", serde_json::json!({"ok": true}))),
        ev(s, ids[3], assistant("a1")),
        ev(s, ids[4], assistant("a2")),
        ev(s, ids[5], assistant("a3")),
    ];
    let policy = CompressPolicy {
        tail_turns: 1,
        preserve_recent_tokens: 0,
        large_output_bytes: 0,
    };
    let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
    assert_eq!(disposition_of(&manifest, ids[5]), Disposition::Verbatim);
    assert_eq!(disposition_of(&manifest, ids[4]), Disposition::Summarized);
    assert_eq!(disposition_of(&manifest, ids[3]), Disposition::Summarized);
    assert_eq!(disposition_of(&manifest, ids[0]), Disposition::Summarized);
    assert!(manifest.summary.is_some());
}

#[tokio::test]
async fn test_large_result_keeps_round() {
    // Compress no longer assigns Referenced — a large tool_result is
    // externalized at the Isolate stage (PostToolUse), before this plan
    // is built. By Compress time the result event already carries a small
    // block_ref marker, and the round keeps its Verbatim/Summarized
    // disposition. Here the big result sits before the verbatim boundary,
    // so the whole round is Summarized; the call and result share it
    // (thinking + tool_use + tool_result integral, never split).
    let s = SessionId::new();
    let ids = ids(5);
    let big = serde_json::json!({"out": "x".repeat(200)});
    let events = vec![
        ev(s, ids[0], user("do work")),
        ev(s, ids[1], call("c1", "run")),
        ev(s, ids[2], result("c1", big)),
        ev(s, ids[3], assistant("done")),
        ev(s, ids[4], assistant("more")),
    ];
    let policy = CompressPolicy {
        tail_turns: 1,
        preserve_recent_tokens: 0,
        large_output_bytes: 50,
    };
    let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
    assert_eq!(disposition_of(&manifest, ids[1]), Disposition::Summarized);
    assert_eq!(disposition_of(&manifest, ids[2]), Disposition::Summarized);
    assert_eq!(
        group_of(&manifest, ids[1]),
        group_of(&manifest, ids[2]),
        "tool_use and its tool_result share a group"
    );
}

#[tokio::test]
async fn test_large_verbatim_kept() {
    // A large result in the verbatim tail stays Verbatim — no Referenced
    // override at Compress. The round is integral; the result event
    // (already a block_ref marker if Isolate ran) rides with it.
    let s = SessionId::new();
    let ids = ids(5);
    let big = serde_json::json!({"out": "z".repeat(200)});
    let events = vec![
        ev(s, ids[0], user("task")),
        ev(s, ids[1], assistant("a1")),
        ev(s, ids[2], call("c1", "run")),
        ev(s, ids[3], result("c1", big)),
        ev(s, ids[4], assistant("a2")),
    ];
    let policy = CompressPolicy {
        tail_turns: 2,
        preserve_recent_tokens: 0,
        large_output_bytes: 50,
    };
    let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
    assert_eq!(disposition_of(&manifest, ids[2]), Disposition::Verbatim);
    assert_eq!(disposition_of(&manifest, ids[3]), Disposition::Verbatim);
    // thinking (a1) and its tool_use (c1) share a group — the
    // turn-atomic property the per-turn-group plan guarantees.
    assert_eq!(
        group_of(&manifest, ids[1]),
        group_of(&manifest, ids[2]),
        "thinking and tool_use share a group"
    );
}

#[tokio::test]
async fn test_pair_integral() {
    // A round with a big result and a small result: the big one is not
    // Referenced at Compress (Isolate owns that). Both pairs sit in
    // their round's group, integral. The verbatim tail is the last
    // assistant turn.
    let s = SessionId::new();
    let ids = ids(7);
    let big = serde_json::json!({"out": "y".repeat(300)});
    let small = serde_json::json!({"ok": true});
    let events = vec![
        ev(s, ids[0], user("first")),
        ev(s, ids[1], call("big", "run")),
        ev(s, ids[2], result("big", big)),
        ev(s, ids[3], assistant("a1")),
        ev(s, ids[4], call("sm", "run")),
        ev(s, ids[5], result("sm", small)),
        ev(s, ids[6], assistant("a2")),
    ];
    let policy = CompressPolicy {
        tail_turns: 1,
        preserve_recent_tokens: 0,
        large_output_bytes: 50,
    };
    let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
    assert_eq!(disposition_of(&manifest, ids[1]), Disposition::Summarized);
    assert_eq!(disposition_of(&manifest, ids[2]), Disposition::Summarized);
    assert_eq!(disposition_of(&manifest, ids[4]), Disposition::Summarized);
    assert_eq!(disposition_of(&manifest, ids[5]), Disposition::Summarized);
    assert_eq!(disposition_of(&manifest, ids[6]), Disposition::Verbatim);
    // Each tool_use and its tool_result share a group (integral pair).
    assert_eq!(group_of(&manifest, ids[1]), group_of(&manifest, ids[2]));
    assert_eq!(group_of(&manifest, ids[4]), group_of(&manifest, ids[5]));
}

#[tokio::test]
async fn test_empty_events() {
    let policy = CompressPolicy::default();
    let manifest = build_manifest(&[], &policy, &HeuristicSummarizer, None).await;
    assert!(manifest.plan.is_empty());
    assert!(manifest.summary.is_none());
}

#[tokio::test]
async fn test_keeps_all_when_few() {
    let s = SessionId::new();
    let ids = ids(2);
    let events = vec![ev(s, ids[0], user("task")), ev(s, ids[1], assistant("a1"))];
    let policy = CompressPolicy {
        tail_turns: 4,
        preserve_recent_tokens: 0,
        large_output_bytes: 0,
    };
    let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
    assert_eq!(disposition_of(&manifest, ids[0]), Disposition::Verbatim);
    assert_eq!(disposition_of(&manifest, ids[1]), Disposition::Verbatim);
    assert!(manifest.summary.is_none());
}

#[tokio::test]
async fn test_orphan_result_kept() {
    // A tool_result with no matching tool_call still gets the round's
    // disposition (Summarized here — before the verbatim boundary). No
    // Referenced override; the group covers it like any other event.
    let s = SessionId::new();
    let ids = ids(3);
    let big = serde_json::json!({"out": "z".repeat(200)});
    let events = vec![
        ev(s, ids[0], user("task")),
        ev(s, ids[1], result("orphan", big)),
        ev(s, ids[2], assistant("a1")),
    ];
    let policy = CompressPolicy {
        tail_turns: 1,
        preserve_recent_tokens: 0,
        large_output_bytes: 50,
    };
    let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
    assert_eq!(disposition_of(&manifest, ids[1]), Disposition::Summarized);
}

#[tokio::test]
async fn test_pending_call_in_group() {
    // A tool_call with no result yet (pending approval or interrupted)
    // still lands in a group with its disposition; the structural guard
    // (every non-delta event covered) holds.
    let s = SessionId::new();
    let ids = ids(3);
    let events = vec![
        ev(s, ids[0], user("task")),
        ev(s, ids[1], call("pending", "run")),
        ev(s, ids[2], assistant("a1")),
    ];
    let policy = CompressPolicy::default();
    let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
    assert!(manifest.plan.iter().any(|g| g.event_ids.contains(&ids[1])));
}

#[tokio::test]
async fn test_policy_knobs_affect_boundary() {
    let s = SessionId::new();
    let ids = ids(6);
    let events = vec![
        ev(s, ids[0], user("task")),
        ev(s, ids[1], assistant("a1")),
        ev(s, ids[2], assistant("a2")),
        ev(s, ids[3], assistant("a3")),
        ev(s, ids[4], assistant("turn four")),
        ev(s, ids[5], assistant("x")),
    ];
    let one_turn = CompressPolicy {
        tail_turns: 1,
        preserve_recent_tokens: 0,
        large_output_bytes: 0,
    };
    let manifest = build_manifest(&events, &one_turn, &HeuristicSummarizer, None).await;
    assert_eq!(disposition_of(&manifest, ids[5]), Disposition::Verbatim);
    assert_eq!(disposition_of(&manifest, ids[4]), Disposition::Summarized);

    let two_turns = CompressPolicy {
        tail_turns: 2,
        preserve_recent_tokens: 0,
        large_output_bytes: 0,
    };
    let manifest = build_manifest(&events, &two_turns, &HeuristicSummarizer, None).await;
    assert_eq!(disposition_of(&manifest, ids[4]), Disposition::Verbatim);
    assert_eq!(disposition_of(&manifest, ids[3]), Disposition::Summarized);

    let shrink = CompressPolicy {
        tail_turns: 2,
        preserve_recent_tokens: 1,
        large_output_bytes: 0,
    };
    let manifest = build_manifest(&events, &shrink, &HeuristicSummarizer, None).await;
    assert_eq!(disposition_of(&manifest, ids[5]), Disposition::Verbatim);
    assert_eq!(disposition_of(&manifest, ids[4]), Disposition::Summarized);
}

#[tokio::test]
async fn test_heuristic_summarizer_nonempty() {
    let s = SessionId::new();
    let events = vec![
        ev(s, EventId::new(), user("hi")),
        ev(s, EventId::new(), assistant("hello")),
    ];
    let summary = HeuristicSummarizer.summarize(&events, None).await.unwrap();
    assert!(!summary.is_empty());
    assert!(summary.contains("1"));
}

#[test]
fn test_reasoning_skipped_in_estimate() {
    let tokenizer = Tokenizer::new();
    let event = SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::Reasoning {
            text: "let me think carefully".into(),
        },
    };
    assert_eq!(estimate_event_tokens(&event, &tokenizer), 0);
}

#[test]
fn test_manifest_shares_model_tokenizer() {
    let tokenizer = Tokenizer::new();
    let cjk = "你好世界 this is mixed content";
    let ev = SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput { text: cjk.into() },
    };
    let estimate = estimate_event_tokens(&ev, &tokenizer);
    let model_count = tokenizer.count(cjk) as usize;
    assert_eq!(estimate, model_count);
}

#[test]
fn test_estimate_counts_each_kind() {
    let tokenizer = Tokenizer::new();
    let s = SessionId::new();
    let mk = |kind: SessionEvent| SessionLogEntry {
        id: EventId::new(),
        session: s,
        ts: 0,
        prev_hash: None,
        event: kind,
    };
    use SessionEvent::*;
    assert!(
        estimate_event_tokens(
            &mk(ToolCall {
                call_id: "c".into(),
                tool: "bash".into(),
                input: serde_json::json!({"cmd": "ls"})
            }),
            &tokenizer
        ) > 0,
        "ToolCall input counted"
    );
    assert!(
        estimate_event_tokens(
            &mk(ToolResult {
                call_id: "c".into(),
                output: serde_json::json!({"out": "data"}),
                duration_ms: 0
            }),
            &tokenizer
        ) > 0,
        "ToolResult output counted"
    );
    assert_eq!(
        estimate_event_tokens(
            &mk(CompactionBoundary {
                checkpoint: houyicoder_context::CheckpointId::new()
            }),
            &tokenizer
        ),
        0,
        "CompactionBoundary is non-text"
    );
    assert_eq!(
        estimate_event_tokens(
            &mk(PermissionDecision {
                call_id: "c".into(),
                tool: "bash".into(),
                verdict: houyicoder_context::PermissionVerdict::Approved,
                scope: "once".into()
            }),
            &tokenizer
        ),
        0,
        "PermissionDecision is non-text"
    );
    assert_eq!(
        estimate_event_tokens(
            &mk(Summary {
                text: "old turns".into()
            }),
            &tokenizer
        ),
        0,
        "durable Summary is replaced by the manifest projection"
    );
    assert_eq!(
        estimate_event_tokens(
            &mk(TurnAborted {
                reason: "timeout".into()
            }),
            &tokenizer
        ),
        0,
        "TurnAborted is not model input"
    );
    assert_eq!(
        estimate_event_tokens(&mk(Unknown), &tokenizer),
        0,
        "Unknown is non-text (future-binary event)"
    );
}
