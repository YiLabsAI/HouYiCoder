//! Model switches inside one user turn: what the call records state, how the
//! chain reads, and how a cache break is represented.

use super::*;

/// A model switch inside one user turn is stated on the call that switched,
/// and a cache break is its own record. The turn's timeline then explains why
/// the reused prefix dropped instead of leaving it to the session totals.
#[test]
fn test_intra_turn_switch_stated() {
    let usage = |ts: u64, model: &str, cache_read: u64| {
        ev(
            ts,
            SessionEvent::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 1_000,
                output_tokens: 10,
                cache_read_input_tokens: cache_read,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: model.into(),
                recovery: false,
                effort: None,
                input_reported: true,
                cache_read_reported: true,
                cache_write_reported: false,
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
        ev(
            110,
            SessionEvent::AssistantMessage {
                text: String::new(),
                thinking: Some("first".into()),
            },
        ),
        usage(120, "pro", 900),
        ev(
            125,
            SessionEvent::TurnStarted {
                turn: 2,
                call_in_turn: 0,
            },
        ),
        ev(
            130,
            SessionEvent::AssistantMessage {
                text: String::new(),
                thinking: Some("second".into()),
            },
        ),
        usage(140, "flash", 0),
        ev(
            145,
            SessionEvent::CacheBreak {
                cause: "model-switch".into(),
            },
        ),
        ev(150, SessionEvent::RunCompleted { ms: Some(100) }),
    ];
    let view = build_view(&events, "test", 0);
    assert_eq!(
        view.total_turns, 1,
        "the second call stays in one user turn"
    );
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(turn.models, vec!["pro".to_string(), "flash".to_string()]);
    assert!(
        turn.boundary_before.is_empty(),
        "a switch inside the turn is not a separator above it: {:?}",
        turn.boundary_before
    );
    let records = records_of(&events, turn.n);
    assert!(
        records.iter().any(|r| r.kind == TrajectoryRecordKind::Model
            && r.name.as_deref() == Some("flash")
            && r.model_switch_from.as_deref() == Some("pro")),
        "the switching call does not carry the switch: {records:#?}"
    );
    assert!(
        records.iter().any(|r| r.kind == TrajectoryRecordKind::Cache
            && r.summary.contains("cache break: model-switch")),
        "the cache break has no record: {records:#?}"
    );
}

/// A call that switched before producing any content still states the switch:
/// the marker takes the empty summary instead of appending to nothing.
#[test]
fn test_switch_states_empty_summary() {
    let usage = |ts: u64, model: &str| {
        ev(
            ts,
            SessionEvent::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 1_000,
                output_tokens: 10,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: model.into(),
                recovery: false,
                effort: None,
                input_reported: true,
                cache_read_reported: true,
                cache_write_reported: false,
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
        usage(120, "pro"),
        ev(
            125,
            SessionEvent::TurnStarted {
                turn: 2,
                call_in_turn: 0,
            },
        ),
        usage(140, "flash"),
        ev(150, SessionEvent::RunCompleted { ms: Some(100) }),
    ];
    let records = records_of(&events, 1);
    let switched = records
        .iter()
        .find(|r| r.kind == TrajectoryRecordKind::Model && r.name.as_deref() == Some("flash"))
        .unwrap_or_else(|| panic!("no flash call record: {records:#?}"));
    assert_eq!(
        switched.model_switch_from.as_deref(),
        Some("pro"),
        "the switch is stated on the call that made it"
    );
}

/// A turn that goes back to a model it already used still states every
/// switch: the chain keeps repeats, and the model a call switched from is the
/// one the previous call actually ran on, not the last distinct id.
#[test]
fn test_switch_roundtrip_chain() {
    let usage = |ts: u64, model: &str| {
        ev(
            ts,
            SessionEvent::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 100,
                output_tokens: 5,
                cache_read_input_tokens: 50,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: model.into(),
                recovery: false,
                effort: None,
                input_reported: true,
                cache_read_reported: true,
                cache_write_reported: false,
            },
        )
    };
    let mut events = vec![
        ev(100, SessionEvent::UserInput { text: "t1".into() }),
        ev(
            101,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
    ];
    for (i, model) in ["pro", "flash", "pro", "flash"].iter().enumerate() {
        let ts = 110 + i as u64 * 10;
        events.push(ev(
            ts,
            SessionEvent::AssistantMessage {
                text: String::new(),
                thinking: Some(format!("step {i}")),
            },
        ));
        events.push(usage(ts + 1, model));
        if i + 1 < 4 {
            events.push(ev(
                ts + 2,
                SessionEvent::TurnStarted {
                    turn: 1,
                    call_in_turn: 0,
                },
            ));
        }
    }
    events.push(ev(200, SessionEvent::RunCompleted { ms: Some(100) }));
    let view = build_view(&events, "test", 0);
    let turn = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        turn.model_chain,
        vec![
            "pro".to_string(),
            "flash".to_string(),
            "pro".to_string(),
            "flash".to_string()
        ],
        "the chain keeps the order the calls ran in"
    );
    let records = records_of(&events, turn.n);
    let switches: Vec<&str> = records
        .iter()
        .filter(|r| r.kind == TrajectoryRecordKind::Model)
        .filter_map(|r| r.model_switch_from.as_deref())
        .collect();
    assert_eq!(
        switches,
        vec!["pro", "flash", "pro"],
        "every switch, including the way back, is stated"
    );
}
