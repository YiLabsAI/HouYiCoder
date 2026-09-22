//! Tests for the loaded window: tail paging, the projection cache, and the
//! session-level totals that the window must not narrow.

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

/// The fold runs over the tail window only, and reports how many turns were
/// left out, so a long session costs the same as a short one.
#[test]
fn test_tail_window_limits() {
    let mut events = Vec::new();
    for i in 0..5 {
        events.push(ev(
            (i as u64) * 1000,
            SessionEvent::UserInput {
                text: format!("prompt {i}"),
            },
        ));
    }
    let view = project(&events, "test", 2);
    assert_eq!(view.rows.len(), 2, "only the newest window is folded");
    assert_eq!(view.hidden_turns, 3, "and the rest is reported as hidden");
    assert_eq!(
        view.total_turns, 5,
        "the session's turn count is a whole-log fact, not the page size"
    );
    let first = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(
        first.user_input, "prompt 3",
        "the window starts at the oldest turn it keeps"
    );
    assert_eq!(
        first.n, 4,
        "the oldest visible turn keeps the number it has in the session"
    );
    // A window wider than the log hides nothing.
    let all = project(&events, "test", 50);
    assert_eq!(all.rows.len(), 5);
    assert_eq!(all.total_turns, 5);
    assert_eq!(all.hidden_turns, 0);
    // Zero means no limit, which is how the fold's own tests read a log.
    let unlimited = project(&events, "test", 0);
    assert_eq!(unlimited.rows.len(), 5);
    assert_eq!(unlimited.total_turns, 5);
    assert_eq!(unlimited.hidden_turns, 0);
}

/// The reader reuses its projection while the log has not changed, and rebuilds
/// it when it has: the pane draws every frame, so an unchanged log must not be
/// re-read and re-folded per draw.
#[test]
fn test_reader_cache_invalidates() {
    use houyicoder_context::{EventId, SessionLogEntry};
    use houyicoder_session::SessionStore;
    use std::sync::Arc;

    let store = Arc::new(SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let sid = houyicoder_context::SessionId::new();
    let reader = SessionLogTrajectory::new(store.clone(), sid, "test".into());

    // Empty log: one turn-less view, stable across reads.
    let first = reader.trajectory();
    assert_eq!(first.total_turns, 0);

    futures::executor::block_on(store.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "hello".into(),
        },
    }))
    .expect("append");

    let after = reader.trajectory();
    assert_eq!(after.total_turns, 1, "the append is picked up");
    let again = reader.trajectory();
    assert_eq!(
        again.total_turns, 1,
        "a second read of an unchanged log gives the same view"
    );
}

/// Asking for older history widens the window, so a turn hidden by the first
/// page appears once the user walks past the oldest loaded turn.
#[test]
fn test_reader_loads_older() {
    use houyicoder_context::{EventId, SessionLogEntry};
    use houyicoder_session::SessionStore;
    use std::sync::Arc;

    let store = Arc::new(SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let sid = houyicoder_context::SessionId::new();
    for i in 0..(TRAJECTORY_PAGE_TURNS + 5) {
        futures::executor::block_on(store.append(SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: (i as u64) * 1000,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: format!("prompt {i}"),
            },
        }))
        .expect("append");
    }
    let reader = SessionLogTrajectory::new(store.clone(), sid, "test".into());
    let first = reader.trajectory();
    assert_eq!(first.rows.len(), TRAJECTORY_PAGE_TURNS);
    assert_eq!(
        first.total_turns,
        TRAJECTORY_PAGE_TURNS + 5,
        "the session's turn count covers the hidden turns too"
    );
    assert_eq!(
        first.hidden_turns, 5,
        "the first page hides the older turns"
    );

    reader.load_older();
    let wider = reader.trajectory();
    assert_eq!(
        wider.total_turns,
        TRAJECTORY_PAGE_TURNS + 5,
        "the next page loads the rest"
    );
    assert_eq!(wider.hidden_turns, 0);
}

/// The session totals cover every turn the session ran, not only the page the
/// rows show. A header that reported the visible page as the session would
/// contradict the status pane and understate the cost of a long session.
#[test]
fn test_session_totals_cover_hidden() {
    let usage = |ts: u64, tin: u64, tout: u64, fail: bool| {
        vec![
            ev(
                ts,
                SessionEvent::TurnUsage {
                    turn: 1,
                    call_in_turn: 1,
                    input_tokens: tin,
                    output_tokens: tout,
                    cache_read_input_tokens: tin / 2,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: 0,
                    model: "test".into(),
                    recovery: false,
                    effort: None,
                },
            ),
            ev(
                ts + 10,
                SessionEvent::ToolResult {
                    call_id: format!("c{ts}"),
                    output: if fail {
                        serde_json::json!({"error": "boom"})
                    } else {
                        serde_json::json!({"ok": true})
                    },
                    duration_ms: 5,
                },
            ),
            ev(
                ts + 20,
                SessionEvent::ModelStepTiming {
                    turn: 1,
                    step: 1,
                    total_ms: 100,
                    ttft_ms: Some(20),
                    decode_ms: Some(80),
                },
            ),
        ]
    };
    let mut events = Vec::new();
    for (i, (tin, tout, fail)) in [(100u64, 10u64, false), (200, 20, true), (300, 30, false)]
        .into_iter()
        .enumerate()
    {
        let ts = 1000 + (i as u64) * 1000;
        events.push(ev(
            ts,
            SessionEvent::UserInput {
                text: format!("prompt {i}"),
            },
        ));
        events.extend(usage(ts + 1, tin, tout, fail));
    }

    let view = project(&events, "test", 1);
    assert_eq!(view.rows.len(), 1, "only one turn is shown");
    assert_eq!(view.hidden_turns, 2);
    assert_eq!(
        view.tokens_in,
        Some(600),
        "the header reports the session, not the page"
    );
    assert_eq!(view.tokens_out, Some(60));
    assert_eq!(view.cache_read, Some(300), "cache totals span the session");
    assert_eq!(view.failures, 1, "a hidden turn's failure still counts");
    assert_eq!(
        view.timing.ttft_samples, 3,
        "the percentiles are computed from the whole session"
    );
    assert_eq!(view.timing.model_ms, 300);
    assert_eq!(view.timing.tool_ms, 15);
    assert_eq!(
        view.duration_secs, 2,
        "the session's wall time is its own event span"
    );
    let first = match &view.rows[0] {
        TrajectoryRow::Turn(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(first.n, 3, "the visible turn keeps its session number");
}

/// Delegated usage is summed from the children's own returns, and a session
/// that delegated nothing reports nothing: the row must not appear as zeroes.
#[test]
fn test_delegated_usage_summed() {
    let child = |id: &str, tin: u64, tout: u64, cache: u64| {
        ev(
            500,
            SessionEvent::SubagentReturn {
                child_session_id: id.into(),
                status: "completed".into(),
                summary: "done".into(),
                result_ref: id.into(),
                input_tokens: tin,
                output_tokens: tout,
                cache_read_input_tokens: cache,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
            },
        )
    };
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
        child("child-1", 180_000, 4_000, 170_000),
        child("child-2", 32_000, 1_000, 30_000),
    ];
    let view = project(&events, "test", 0);
    let delegated = view.delegated.expect("two returns produce a delegated row");
    assert_eq!(delegated.calls, 2);
    assert_eq!(delegated.input, 212_000, "the children's input is summed");
    assert_eq!(delegated.output, 5_000);
    assert_eq!(delegated.cache_read, 200_000);
    assert_eq!(
        delegated.cache_hit_pct(),
        Some(200_000.0 / 212_000.0 * 100.0)
    );
    // The children's tokens are not folded into the session totals, which come
    // from the parent's own calls.
    assert_eq!(
        view.tokens_in, None,
        "the parent made no calls, so the session total stays unknown"
    );
}

/// A session with no delegation reports no delegated usage at all.
#[test]
fn test_delegated_usage_absent() {
    let events = vec![ev(
        100,
        SessionEvent::UserInput {
            text: "just talk".into(),
        },
    )];
    let view = project(&events, "test", 0);
    assert!(view.delegated.is_none(), "no children, no row");
}

/// A child that reported no usage leaves the row without a cache share rather
/// than a fabricated one.
#[test]
fn test_delegated_usage_no_input() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "explore".into(),
            },
        ),
        ev(
            500,
            SessionEvent::SubagentReturn {
                child_session_id: "child-1".into(),
                status: "timeout".into(),
                summary: String::new(),
                result_ref: "child-1".into(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
            },
        ),
    ];
    let view = project(&events, "test", 0);
    let delegated = view.delegated.expect("the call is still reported");
    assert_eq!(delegated.calls, 1);
    assert_eq!(delegated.input, 0);
    assert_eq!(
        delegated.cache_hit_pct(),
        None,
        "an unknown share is absent, not zero"
    );
}
