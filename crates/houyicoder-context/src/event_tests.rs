//! Wire-type tests for the logged record: the event shape, the verdict
//! enums, and the serde round-trips that keep an old log readable.

#![cfg(test)]

use super::*;

fn event(session: SessionId, id: EventId, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id,
        session,
        ts: 0,
        prev_hash: None,
        event: kind,
    }
}

#[test]
fn test_event_serde_round_trip() {
    let s = SessionId::new();
    let e = event(
        s,
        EventId::new(),
        SessionEvent::UserInput { text: "hi".into() },
    );
    let json = serde_json::to_string(&e).expect("serialize");
    let back: SessionLogEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, e);
    assert!(json.contains("\"type\":\"UserInput\""));
}

#[test]
fn test_subagent_spawn_round_trip() {
    let s = SessionId::new();
    let e = event(
        s,
        EventId::new(),
        SessionEvent::SubagentSpawn {
            child_session_id: "child-1".into(),
            subagent_type: "explore".into(),
            prompt_summary: "find the auth module".into(),
            isolation: "worktree".into(),
            policy: "delegate".into(),
            trigger_source: "model:call-1".into(),
        },
    );
    let json = serde_json::to_string(&e).expect("serialize");
    let back: SessionLogEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, e);
    assert!(json.contains("\"type\":\"SubagentSpawn\""));
    assert!(
        json.contains("\"trigger_source\":\"model:call-1\""),
        "trigger_source round-trips so a replay distinguishes the origin: {json}"
    );
}

#[test]
fn test_subagent_return_round_trip() {
    let s = SessionId::new();
    let e = event(
        s,
        EventId::new(),
        SessionEvent::SubagentReturn {
            child_session_id: "child-1".into(),
            status: "completed".into(),
            summary: "auth lives in crates/api".into(),
            result_ref: "evt-42".into(),
            input_tokens: 800,
            output_tokens: 400,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        },
    );
    let json = serde_json::to_string(&e).expect("serialize");
    let back: SessionLogEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, e);
    assert!(json.contains("\"type\":\"SubagentReturn\""));
}

#[test]
fn test_notification_round_trip() {
    let s = SessionId::new();
    let e = event(
        s,
        EventId::new(),
        SessionEvent::NotificationInjected {
            child_session_id: "child-1".into(),
            turn: 3,
            order: 1,
            topic: "task.child-1.completed".into(),
            summary: "Subagent explore completed: found auth".into(),
        },
    );
    let json = serde_json::to_string(&e).expect("serialize");
    let back: SessionLogEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, e);
    assert!(json.contains("\"type\":\"NotificationInjected\""));
    assert!(
        json.contains("Subagent explore completed: found auth"),
        "summary round-trips: {json}"
    );
}

/// An older log written before the summary field existed must still
/// deserialize (the field is #[serde(default)]) so a version bump does not
/// brick prior sessions. Build a real event's JSON, strip the summary key,
/// and confirm the missing field defaults to empty rather than failing.
#[test]
fn test_notification_old_log_deserializes() {
    let s = SessionId::new();
    let e = event(
        s,
        EventId::new(),
        SessionEvent::NotificationInjected {
            child_session_id: "child-2".into(),
            turn: 1,
            order: 0,
            topic: "completion".into(),
            summary: "to be stripped".into(),
        },
    );
    let json = serde_json::to_string(&e).expect("serialize");
    // Strip the summary field so the JSON looks like a pre-summary log line.
    let value = serde_json::from_str::<serde_json::Value>(&json).expect("parse");
    let kind = value
        .get("event")
        .and_then(serde_json::Value::as_object)
        .expect("event object");
    let mut kind_obj = kind.clone();
    kind_obj.remove("summary");
    let mut wrapped = serde_json::Map::new();
    if let serde_json::Value::Object(top) = value {
        for (k, v) in top {
            if k == "event" {
                wrapped.insert("event".into(), serde_json::Value::Object(kind_obj.clone()));
            } else {
                wrapped.insert(k, v);
            }
        }
    }
    let old = serde_json::to_string(&serde_json::Value::Object(wrapped)).expect("re-serialize");
    let back: SessionLogEntry = serde_json::from_str(&old).expect("old log deserializes");
    match back.event {
        SessionEvent::NotificationInjected {
            child_session_id,
            summary,
            turn,
            ..
        } => {
            assert_eq!(child_session_id, "child-2");
            assert_eq!(turn, 1);
            assert!(summary.is_empty(), "missing summary defaults to empty");
        }
        other => panic!("expected NotificationInjected, got {other:?}"),
    }
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
    // Every SessionEvent variant must survive a serde cycle: the
    // internally-tagged enum plus nested serde_json::Value and CheckpointId.
    let s = SessionId::new();
    let call_id = "toolu_01call";
    let cp = CheckpointId::new();
    let cases = vec![
        event(
            s,
            EventId::new(),
            SessionEvent::AssistantMessage {
                text: "hi".into(),
                thinking: None,
            },
        ),
        event(
            s,
            EventId::new(),
            SessionEvent::AssistantTextDelta { text: "hel".into() },
        ),
        event(
            s,
            EventId::new(),
            SessionEvent::ToolCall {
                call_id: call_id.to_string(),
                tool: "edit".into(),
                input: serde_json::json!({"path": "x.rs", "line": 3}),
            },
        ),
        event(
            s,
            EventId::new(),
            SessionEvent::ToolResult {
                call_id: call_id.to_string(),
                output: serde_json::json!(["ok", 42]),
                duration_ms: 0,
            },
        ),
        event(
            s,
            EventId::new(),
            SessionEvent::Reasoning {
                text: "thinking".into(),
            },
        ),
        event(
            s,
            EventId::new(),
            SessionEvent::CompactionBoundary {
                checkpoint: cp,
                pre_tokens: 0,
                post_tokens: 0,
            },
        ),
        event(
            s,
            EventId::new(),
            SessionEvent::Summary {
                text: "head summarized".into(),
            },
        ),
        event(
            s,
            EventId::new(),
            SessionEvent::PermissionDecision {
                call_id: call_id.to_string(),
                tool: "bash".into(),
                verdict: PermissionVerdict::Approved,
                scope: "once".into(),
            },
        ),
        event(
            s,
            EventId::new(),
            SessionEvent::TruncationVerdict {
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
            SessionEvent::TurnUsage {
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
            SessionEvent::HookSignal {
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
        event(
            s,
            EventId::new(),
            SessionEvent::SkillListing {
                text: "- commit: commit changes".into(),
                bytes: 26,
                content_hash: 0xdeadbeef,
            },
        ),
        event(
            s,
            EventId::new(),
            SessionEvent::RunCompleted { ms: Some(620) },
        ),
        event(s, EventId::new(), SessionEvent::RunCompleted { ms: None }),
    ];
    for e in &cases {
        let json = serde_json::to_string(e).expect("serialize");
        let back: SessionLogEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, *e);
    }
    // CompactionBoundary carries the nested CheckpointId — verify it.
    let json = serde_json::to_string(&cases[5]).unwrap();
    assert!(json.contains("\"checkpoint\""));
    let back: SessionLogEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(back, cases[5]);
    // SkillListing's content_hash is on the wire, and an old-log JSON
    // missing the field deserializes to 0 (forward compat for logs written
    // before the field existed).
    let sl = cases
        .iter()
        .find(|e| matches!(e.event, SessionEvent::SkillListing { .. }))
        .expect("SkillListing case present");
    let sl_json = serde_json::to_string(sl).unwrap();
    assert!(
        sl_json.contains("\"content_hash\""),
        "hash on the wire: {sl_json}"
    );
    let mut v: serde_json::Value = serde_json::from_str(&sl_json).unwrap();
    if let Some(serde_json::Value::Object(kind_map)) = v.get_mut("event") {
        kind_map.remove("content_hash");
    }
    let legacy: SessionLogEntry = serde_json::from_value(v).expect("legacy deserialize");
    match legacy.event {
        SessionEvent::SkillListing {
            content_hash: 0, ..
        } => {}
        other => panic!("expected legacy SkillListing with hash 0, got {other:?}"),
    }
    // A turn whose loop was never measured records no duration, and a record
    // written before the field existed carries it in the stored form alone.
    // Both read as unknown, never as a turn that took no time.
    let untimed = cases
        .iter()
        .find(|e| matches!(e.event, SessionEvent::RunCompleted { ms: None }))
        .expect("unmeasured record case present");
    let json = serde_json::to_string(untimed).unwrap();
    assert!(
        json.contains("\"RunCompleted\""),
        "named in the stored form: {json}"
    );
    let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
    if let Some(serde_json::Value::Object(fields)) = v.get_mut("event") {
        fields.remove("ms");
    }
    let older: SessionLogEntry = serde_json::from_value(v).expect("legacy deserialize");
    match older.event {
        SessionEvent::RunCompleted { ms: None } => {}
        other => panic!("expected RunCompleted with no duration, got {other:?}"),
    }
    // A line written while the field was still named secs. The stored bytes
    // are spelled out rather than edited out of a current serialization, so
    // this decodes an old line and not a mutated new one: a duration under a
    // name this shape does not carry reads as unknown, losing the label
    // rather than printing a figure that means something else now.
    let legacy: SessionEvent =
        serde_json::from_str(r#"{"type":"RunCompleted","secs":7}"#).expect("an old line decodes");
    match legacy {
        SessionEvent::RunCompleted { ms: None } => {}
        other => panic!("expected a duration under the old name to read as unknown, got {other:?}"),
    }
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
    let verdict = SessionEvent::TruncationVerdict {
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
    let back: SessionLogEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, e);
    // Distinguish the two: raw is the provider dialect, normalized is the
    // flattened form. They must not collapse into one field.
    if let SessionEvent::TruncationVerdict {
        raw_finish_reason,
        normalized_reason,
        ..
    } = back.event
    {
        assert_eq!(raw_finish_reason.as_deref(), Some("max_tokens"));
        assert_eq!(normalized_reason.as_deref(), Some("length"));
        assert_ne!(raw_finish_reason, normalized_reason);
    } else {
        panic!("expected TruncationVerdict");
    }
}

#[test]
fn test_timing_and_clear_events() {
    let s = SessionId::new();
    let timing = SessionEvent::ModelStepTiming {
        turn: 1,
        step: 0,
        total_ms: 1250,
        ttft_ms: Some(350),
        decode_ms: Some(900),
        reasoning_ms: Some(400),
        response_ms: Some(500),
    };
    let e1 = event(s, EventId::new(), timing);
    let json1 = serde_json::to_string(&e1).expect("serialize");
    assert!(json1.contains("\"type\":\"ModelStepTiming\""));
    assert!(json1.contains("\"ttft_ms\":350"));
    assert!(json1.contains("\"reasoning_ms\":400"));
    let back1: SessionLogEntry = serde_json::from_str(&json1).expect("deserialize");
    assert_eq!(back1, e1);

    let cleared = SessionEvent::ContextCleared { prior_turn: 2 };
    let e2 = event(s, EventId::new(), cleared);
    let json2 = serde_json::to_string(&e2).expect("serialize");
    assert!(json2.contains("\"type\":\"ContextCleared\""));
    assert!(json2.contains("\"prior_turn\":2"));
    let back2: SessionLogEntry = serde_json::from_str(&json2).expect("deserialize");
    assert_eq!(back2, e2);
}

/// An entry whose spans were never recorded carries neither key, and reads back
/// with both unknown: an absent figure is not a zero, and a reader that
/// defaulted them to zero would report a model that thought for no time.
#[test]
fn test_timing_spans_absent_unknown() {
    let s = SessionId::new();
    let event = event(
        s,
        EventId::new(),
        SessionEvent::ModelStepTiming {
            turn: 1,
            step: 0,
            total_ms: 1250,
            ttft_ms: Some(350),
            decode_ms: Some(900),
            reasoning_ms: None,
            response_ms: None,
        },
    );
    let json = serde_json::to_string(&event).expect("serialize");
    assert!(
        !json.contains("reasoning_ms") && !json.contains("response_ms"),
        "an unknown span is written as no key at all: {json}"
    );

    let back: SessionLogEntry = serde_json::from_str(&json).expect("deserialize");
    match back.event {
        SessionEvent::ModelStepTiming {
            reasoning_ms,
            response_ms,
            ..
        } => {
            assert_eq!(reasoning_ms, None, "no reasoning span was recorded");
            assert_eq!(response_ms, None, "and no reply span either");
        }
        other => panic!("a timing event: {other:?}"),
    }
}
