//! TurnUsage effort-field tests, split from lib.rs to keep that file under
//! the size gate. The effort field is inlined as a string (the context crate
//! stays a serde-only leaf with no protocol dep); the wire form matches the
//! EffortLevel enum's lowercase serde so a typed caller round-trips through
//! the same bytes. Old logs that predate the field deserialize to None.

#![cfg(test)]

use crate::{EventId, SessionId, TurnEvent, TurnEventKind};

fn event(session: SessionId, id: EventId, kind: TurnEventKind) -> TurnEvent {
    TurnEvent {
        id,
        session,
        ts: 0,
        prev_hash: None,
        kind,
    }
}

/// An old log entry that predates the effort field still deserializes: effort
/// defaults to None (unknown), never a fake level. Built by serializing a real
/// event then stripping the effort key, so the ids are valid shapes.
#[test]
fn test_effort_absent_defaults_none() {
    let s = SessionId::new();
    let e = event(
        s,
        EventId::new(),
        TurnEventKind::TurnUsage {
            turn: 1,
            call_in_turn: 1,
            input_tokens: 100,
            output_tokens: 50,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            model: "test".into(),
            recovery: false,
            effort: Some("high".into()),
        },
    );
    let json = serde_json::to_string(&e).expect("serialize");
    // Strip the effort field to mimic a log written before the field existed.
    let legacy = json.replace(r#","effort":"high""#, "");
    assert!(!legacy.contains("effort"), "effort stripped: {legacy}");
    let back: TurnEvent = serde_json::from_str(&legacy).expect("deserialize");
    match back.kind {
        TurnEventKind::TurnUsage { effort, .. } => {
            assert!(effort.is_none(), "absent effort => None, not a default");
        }
        _ => unreachable!(),
    }
}

/// The effort string survives a serde cycle for None and each level.
#[test]
fn test_usage_effort_round_trips() {
    let s = SessionId::new();
    for effort in [Some("low"), Some("medium"), Some("high"), None] {
        let e = event(
            s,
            EventId::new(),
            TurnEventKind::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "test".into(),
                recovery: false,
                effort: effort.map(str::to_string),
            },
        );
        let json = serde_json::to_string(&e).expect("serialize");
        let back: TurnEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, e);
    }
}

/// An unknown event type deserializes to Unknown rather than failing, so an
/// old binary can read a log a newer binary wrote.
#[test]
fn test_unknown_type_deserializes() {
    let json = serde_json::json!({"type": "Garbage", "text": "x"});
    let kind: TurnEventKind = serde_json::from_value(json).unwrap();
    assert!(matches!(kind, TurnEventKind::Unknown));
}
