//! Wire-type tests for the logged record: the event shape, the verdict
//! enums, and the serde round-trips that keep an old log readable.

#![cfg(test)]

use super::*;

fn event(session: SessionId, id: EventId, kind: TurnEventKind) -> TurnEvent {
    TurnEvent {
        id,
        session,
        ts: 0,
        prev_hash: None,
        kind,
    }
}

#[test]
fn test_event_serde_round_trip() {
    let s = SessionId::new();
    let e = event(
        s,
        EventId::new(),
        TurnEventKind::UserInput { text: "hi".into() },
    );
    let json = serde_json::to_string(&e).expect("serialize");
    let back: TurnEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, e);
    assert!(json.contains("\"type\":\"UserInput\""));
}

#[test]
fn test_session_id_round_trips() {
    // A freshly minted SessionId serializes as a hyphenated UUID and
    // parses back to the same value, so a round trip is lossless.
    let s = SessionId::new();
    let display = s.to_string();
    assert!(
        display.len() == 36 && display.matches('-').count() == 4,
        "session id should be a hyphenated UUID, got {display}",
    );
    assert_eq!(SessionId::from_display_string(&display), Some(s));
}

#[test]
fn test_session_id_accepts_ulid() {
    // A pre-change export carries ULID session ids in each event.
    // Deserialize must accept the legacy form so an old session log
    // resumes after the sid-format change; the ULID's 128 bits become
    // the Uuid's bits (value identity, not string identity).
    let legacy = "01KZ5RDH4DG6YV0EDBX1KSKTRA";
    let parsed = SessionId::from_display_string(legacy);
    assert!(parsed.is_some(), "legacy ULID should parse: {legacy}");
    let sid = parsed.unwrap();
    // Serialize is forward-only (hyphenated UUID), so the string form
    // changes on reserialize -- the value is preserved, the costume is not.
    assert_ne!(sid.to_string(), legacy);
    // The same 128 bits: round-trip the reserialized form back.
    assert_eq!(SessionId::from_display_string(&sid.to_string()), Some(sid));
    // Tolerant via the Deserialize impl too (the path export import takes).
    let json = format!("\"{legacy}\"");
    let de: SessionId = serde_json::from_str(&json).expect("deserialize legacy ULID");
    assert_eq!(de, sid);
}

#[test]
fn test_session_id_rejects_garbage() {
    assert!(SessionId::from_display_string("not-a-session-id").is_none());
}

#[test]
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
fn test_event_variants_round_trip() {
    // Every TurnEventKind variant must survive a serde cycle: the
    // internally-tagged enum plus nested serde_json::Value and CheckpointId.
    let s = SessionId::new();
    let call_id = "toolu_01call";
    let cp = CheckpointId::new();
    let cases = vec![
        event(
            s,
            EventId::new(),
            TurnEventKind::AssistantMessage {
                text: "hi".into(),
                thinking: None,
            },
        ),
        event(
            s,
            EventId::new(),
            TurnEventKind::AssistantTextDelta { text: "hel".into() },
        ),
        event(
            s,
            EventId::new(),
            TurnEventKind::ToolCall {
                call_id: call_id.to_string(),
                tool: "edit".into(),
                input: serde_json::json!({"path": "x.rs", "line": 3}),
            },
        ),
        event(
            s,
            EventId::new(),
            TurnEventKind::ToolResult {
                call_id: call_id.to_string(),
                output: serde_json::json!(["ok", 42]),
                duration_ms: 0,
            },
        ),
        event(
            s,
            EventId::new(),
            TurnEventKind::Reasoning {
                text: "thinking".into(),
            },
        ),
        event(
            s,
            EventId::new(),
            TurnEventKind::CompactionBoundary { checkpoint: cp },
        ),
        event(
            s,
            EventId::new(),
            TurnEventKind::Summary {
                text: "head summarized".into(),
            },
        ),
        event(
            s,
            EventId::new(),
            TurnEventKind::PermissionDecision {
                call_id: call_id.to_string(),
                tool: "bash".into(),
                verdict: PermissionVerdict::Approved,
                scope: "once".into(),
            },
        ),
        event(
            s,
            EventId::new(),
            TurnEventKind::TruncationVerdict {
                raw_finish_reason: Some("max_tokens".into()),
                normalized_reason: Some("length".into()),
                signal: TruncationSignal::ServerUsageNearCap,
                server_output_tokens: 8_000,
                self_count_output_tokens: 7_950,
                max_output_tokens: 8_000,
                recovery_attempts: 1,
                recovery_fired: true,
            },
        ),
        event(
            s,
            EventId::new(),
            TurnEventKind::TurnUsage {
                turn: 3,
                call_in_turn: 2,
                input_tokens: 1000,
                output_tokens: 500,
                cache_read_input_tokens: 800,
                cache_write_input_tokens: 50,
                reasoning_tokens: 100,
                model: "test".into(),
                recovery: true,
                effort: Some("high".into()),
            },
        ),
        event(
            s,
            EventId::new(),
            TurnEventKind::HookSignal {
                event: HookEventKind::PreToolUse,
                verdict: HookVerdictKind::Deny,
                error: Some(HookErrorKind::Timeout),
                reason: "off-limits".into(),
                hook_name: "deny-bash".into(),
                tool_name: Some("bash".into()),
                triggered_event: None,
                turn: Some(3),
                call_in_turn: Some(2),
            },
        ),
    ];
    for e in &cases {
        let json = serde_json::to_string(e).expect("serialize");
        let back: TurnEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, *e);
    }
    // CompactionBoundary carries the nested CheckpointId — verify it.
    let json = serde_json::to_string(&cases[5]).unwrap();
    assert!(json.contains("\"checkpoint\""));
    let back: TurnEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(back, cases[5]);
}

#[test]
fn test_truncation_signal_round_trips() {
    for signal in [
        TruncationSignal::ServerUsageNearCap,
        TruncationSignal::SelfCountNearCap,
        TruncationSignal::UnclosedCodeBlock,
        TruncationSignal::None,
    ] {
        let json = serde_json::to_string(&signal).expect("serialize");
        let back: TruncationSignal = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, signal);
    }
}

#[test]
fn test_verdict_preserves_raw_dialect() {
    // The raw finish_reason and the normalized reason must both survive a
    // serde cycle as distinct values: the raw carries the provider dialect
    // (max_tokens) while the normalized carries the flattened form (length)
    // the drive loop keys on. If the raw is lost, trajectory analysis
    // cannot tell which gateway spelling triggered the cut.
    let verdict = TurnEventKind::TruncationVerdict {
        raw_finish_reason: Some("max_tokens".into()),
        normalized_reason: Some("length".into()),
        signal: TruncationSignal::ServerUsageNearCap,
        server_output_tokens: 8_000,
        self_count_output_tokens: 0,
        max_output_tokens: 8_000,
        recovery_attempts: 2,
        recovery_fired: false,
    };
    let e = event(SessionId::new(), EventId::new(), verdict);
    let json = serde_json::to_string(&e).expect("serialize");
    assert!(json.contains("\"raw_finish_reason\":\"max_tokens\""));
    assert!(json.contains("\"normalized_reason\":\"length\""));
    let back: TurnEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, e);
    // Distinguish the two: raw is the provider dialect, normalized is the
    // flattened form. They must not collapse into one field.
    if let TurnEventKind::TruncationVerdict {
        raw_finish_reason,
        normalized_reason,
        ..
    } = back.kind
    {
        assert_eq!(raw_finish_reason.as_deref(), Some("max_tokens"));
        assert_eq!(normalized_reason.as_deref(), Some("length"));
        assert_ne!(raw_finish_reason, normalized_reason);
    } else {
        panic!("expected TruncationVerdict");
    }
}
