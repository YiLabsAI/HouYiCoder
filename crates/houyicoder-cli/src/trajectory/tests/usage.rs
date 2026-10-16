//! Tests for the turn's usage totals: which counts the durable log hands the
//! fold, and the cache share the account behind them does or does not support.

use super::super::view::build_view;
use super::view::{ev, usage_ev, usage_ev_unreported};
use houyicoder_context::SessionEvent;
use houyicoder_tui::view::trajectory_pane::{TrajectoryRow, TrajectoryTurn, TrajectoryView};

fn turn_of(view: &TrajectoryView, index: usize) -> &TrajectoryTurn {
    match &view.rows[index] {
        TrajectoryRow::Turn(turn) => turn,
        TrajectoryRow::Bg(_) => unreachable!("the row at the index is a turn"),
    }
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
    let view = build_view(&events, "test", 0);
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

/// A count a log reported no flag for is unknown, not a measured zero, so the
/// turn derives no cache share from it.
#[test]
fn test_unreported_call_hides_share() {
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
        usage_ev_unreported(110, 1, 1_000, 0),
    ];
    let view = build_view(&events, "test", 0);
    let turn = turn_of(&view, 0);
    assert_eq!(turn.tokens_in, Some(1_000), "the count is still folded");
    assert_eq!(
        turn.cache_hit_pct(),
        None,
        "an unreported count beneath a zero claims no share"
    );
}

/// One call that reported and one that did not leave the turn's account
/// incomplete: the sum cannot be trusted while an unstated part sits in it. The
/// count the reported call recorded is still folded and still stated — only the
/// share over it is withheld, since a quotient over a short input sum can read
/// higher than the truth.
#[test]
fn test_mixed_report_hides_share() {
    let turn_view = |cached: u64, unstated: u64| {
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
            usage_ev(110, 1, 1_000, 10, cached),
            usage_ev_unreported(120, 1, 1_000, unstated),
        ];
        build_view(&events, "test", 0)
    };
    let unstated_zero = turn_view(0, 0);
    let turn = turn_of(&unstated_zero, 0);
    assert_eq!(turn.tokens_in, Some(2_000), "both calls are folded");
    assert_eq!(
        turn.cache_hit_pct(),
        None,
        "an unstated call leaves the turn's account incomplete"
    );

    let named_count = turn_view(500, 0);
    assert_eq!(
        turn_of(&named_count, 0).cache_read,
        Some(500),
        "the count a call did record is still folded"
    );
    assert_eq!(
        turn_of(&named_count, 0).cache_hit_pct(),
        None,
        "but no share is derived over an account the other call left short"
    );
}

/// Every call reporting the same account is what lets the turn state a share,
/// and the two counts have to be complete together: a turn whose input was
/// reported but whose cache-read count never was states no share either.
#[test]
fn test_complete_report_shows_share() {
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
        usage_ev(110, 1, 1_000, 10, 500),
    ];
    let view = build_view(&events, "test", 0);
    assert_eq!(
        turn_of(&view, 0).cache_hit_pct(),
        Some(50.0),
        "a complete account states its share"
    );
}

/// A child that returned without recording any count leaves the turn's account
/// unknown rather than spent-nothing: the turn must not claim a share over a
/// total that omits work nobody measured.
#[test]
fn test_unmeasured_child_hides_share() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "delegate".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        usage_ev(110, 1, 1_000, 10, 500),
        ev(
            115,
            SessionEvent::SubagentSpawn {
                child_session_id: "child".into(),
                subagent_type: "worker".into(),
                prompt_summary: "go".into(),
                isolation: String::new(),
                policy: String::new(),
                trigger_source: "model:c1".into(),
            },
        ),
        ev(
            120,
            SessionEvent::SubagentReturn {
                child_session_id: "child".into(),
                status: "completed".into(),
                summary: "done".into(),
                result_ref: String::new(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                input_reported: false,
                cache_read_reported: false,
                cache_write_reported: false,
            },
        ),
    ];
    let view = build_view(&events, "test", 0);
    let turn = turn_of(&view, 0);
    assert_eq!(
        turn.tokens_in,
        Some(1_000),
        "a child that recorded nothing adds no tokens"
    );
    assert_eq!(
        turn.cache_hit_pct(),
        None,
        "an unmeasured child leaves the turn's account partial"
    );
}

/// A child that reported cache tokens without input or output spent a real
/// amount: its counts fold into the turn rather than being dropped, and its
/// reporting still governs the turn's share.
#[test]
fn test_cache_only_child_folds() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "delegate".into(),
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
                child_session_id: "child".into(),
                subagent_type: "worker".into(),
                prompt_summary: "go".into(),
                isolation: String::new(),
                policy: String::new(),
                trigger_source: "model:c1".into(),
            },
        ),
        ev(
            120,
            SessionEvent::SubagentReturn {
                child_session_id: "child".into(),
                status: "completed".into(),
                summary: "done".into(),
                result_ref: String::new(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 500,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                input_reported: false,
                cache_read_reported: true,
                cache_write_reported: false,
            },
        ),
    ];
    let view = build_view(&events, "test", 0);
    let turn = turn_of(&view, 0);
    assert_eq!(
        turn.cache_read,
        Some(500),
        "the count the child reported is folded into the turn"
    );
    assert_eq!(
        turn.cache_hit_pct(),
        None,
        "a child that reported no input total leaves the share unstated"
    );
}

/// A child's counts fold into the turn's totals, so the child's own reporting
/// governs whether the turn can claim a share over them.
#[test]
fn test_child_report_hides_share() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "delegate".into(),
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
                child_session_id: "child".into(),
                subagent_type: "worker".into(),
                prompt_summary: "go".into(),
                isolation: String::new(),
                policy: String::new(),
                trigger_source: "model:c1".into(),
            },
        ),
        ev(
            120,
            SessionEvent::SubagentReturn {
                child_session_id: "child".into(),
                status: "completed".into(),
                summary: "done".into(),
                result_ref: String::new(),
                input_tokens: 900,
                output_tokens: 10,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                input_reported: false,
                cache_read_reported: false,
                cache_write_reported: false,
            },
        ),
    ];
    let view = build_view(&events, "test", 0);
    let turn = turn_of(&view, 0);
    assert_eq!(turn.tokens_in, Some(900), "the child's input is folded");
    assert_eq!(
        turn.cache_hit_pct(),
        None,
        "a child that reported no account leaves the turn's share unstated"
    );
}
