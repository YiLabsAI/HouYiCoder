//! TurnUsage effort-field tests, split from lib.rs to keep that file under
//! the size gate. The effort field is inlined as a string (the context crate
//! stays a serde-only leaf with no protocol dep); the wire form matches the
//! EffortLevel enum's lowercase serde so a typed caller round-trips through
//! the same bytes. Old logs that predate the field deserialize to None.

#![cfg(test)]

use crate::{EventId, SessionEvent, SessionId, SessionLogEntry};

fn event(session: SessionId, id: EventId, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id,
        session,
        ts: 0,
        prev_hash: None,
        event: kind,
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
        SessionEvent::TurnUsage {
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
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        },
    );
    let json = serde_json::to_string(&e).expect("serialize");
    // Strip the effort field to mimic a log written before the field existed.
    let legacy = json.replace(r#","effort":"high""#, "");
    assert!(!legacy.contains("effort"), "effort stripped: {legacy}");
    let back: SessionLogEntry = serde_json::from_str(&legacy).expect("deserialize");
    match back.event {
        SessionEvent::TurnUsage { effort, .. } => {
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
            SessionEvent::TurnUsage {
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
                input_reported: true,
                cache_read_reported: true,
                cache_write_reported: false,
            },
        );
        let json = serde_json::to_string(&e).expect("serialize");
        let back: SessionLogEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, e);
    }
}

/// An old log entry that predates the reported-field flags still deserializes:
/// every flag defaults to false, so a tally rebuilt from that log reads the
/// turn as unreported rather than as a complete zero.
#[test]
fn test_flags_absent_read_unreported() {
    let s = SessionId::new();
    let e = event(
        s,
        EventId::new(),
        SessionEvent::TurnUsage {
            turn: 1,
            call_in_turn: 1,
            input_tokens: 100,
            output_tokens: 50,
            cache_read_input_tokens: 80,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            model: "test".into(),
            recovery: false,
            effort: None,
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: true,
        },
    );
    let json = serde_json::to_string(&e).expect("serialize");
    // Strip the flags to mimic a log written before the fields existed.
    let legacy = json
        .replace(r#","input_reported":true"#, "")
        .replace(r#","cache_read_reported":true"#, "")
        .replace(r#","cache_write_reported":true"#, "");
    assert!(!legacy.contains("reported"), "flags stripped: {legacy}");
    let back: SessionLogEntry = serde_json::from_str(&legacy).expect("deserialize");
    match back.event {
        SessionEvent::TurnUsage {
            input_reported,
            cache_read_reported,
            cache_write_reported,
            ..
        } => {
            assert!(
                !input_reported && !cache_read_reported && !cache_write_reported,
                "absent flags read as unreported, not as reported zeroes"
            );
        }
        _ => unreachable!(),
    }
}

/// An unknown event type deserializes to Unknown rather than failing, so an
/// old binary can read a log a newer binary wrote.
#[test]
fn test_unknown_type_deserializes() {
    let json = serde_json::json!({"type": "Garbage", "text": "x"});
    let kind: SessionEvent = serde_json::from_value(json).unwrap();
    assert!(matches!(kind, SessionEvent::Unknown));
}
