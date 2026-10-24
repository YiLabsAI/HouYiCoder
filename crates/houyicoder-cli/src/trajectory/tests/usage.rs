//! Tests for the turn's usage totals and the session account they fold into:
//! which counts the durable log hands the fold, the cache share the account
//! behind them does or does not support, and the same account read through the
//! paged disk path the product runs on.

use super::super::reader::SessionLogTrajectory;
use super::super::view::build_view;
use super::paging::{pump, reader_of};
use super::view::{ev, usage_ev, usage_ev_unreported};
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_memory::LocalFileBackend;
use houyicoder_session::SessionStore;
use houyicoder_tui::view::trajectory_pane::{TrajectoryRow, TrajectoryTurn, TrajectoryView};
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::process::id;
use std::sync::Arc;
use tokio::runtime::Runtime;

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
                tool_calls: 0,
                tool_failures: 0,
                duration_ms: 0,
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
                tool_calls: 0,
                tool_failures: 0,
                duration_ms: 0,
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
                tool_calls: 0,
                tool_failures: 0,
                duration_ms: 0,
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

/// The session's account holds the parent's calls and every child's, and its
/// completeness is one folded fact: the parent reported everything here, the
/// child reported nothing, and the session is still not complete.
#[test]
fn test_child_report_gates_session() {
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
                tool_calls: 0,
                tool_failures: 0,
                duration_ms: 0,
            },
        ),
    ];
    let view = build_view(&events, "test", 0);
    assert!(
        !view.usage_provenance.cache_account_complete(),
        "a parent that reported everything does not cover a child that did not"
    );
    assert_eq!(
        view.tokens_in, None,
        "a child that measured nothing leaves the session total unknown"
    );
}

/// A session whose endpoints reported the cache-read count and reused nothing
/// carries a zero, not an absence: the header derives a measured miss from it
/// rather than staying silent as it does for an unreported count.
#[test]
fn test_reported_zero_is_stated() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "fresh".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        usage_ev(110, 1, 1_000, 10, 0),
    ];
    let view = build_view(&events, "test", 0);
    assert_eq!(
        view.cache_read,
        Some(0),
        "a reported zero is a count the header can state"
    );
    assert!(
        view.usage_provenance.cache_account_complete(),
        "and the account behind it is complete"
    );
}

/// The paged disk path carries the session's completeness beside its counts, so
/// the header can ask whether the account behind them is whole. A log whose call
/// reported nothing leaves the share silent rather than deriving it over an
/// input total short by whatever went unreported.
#[test]
fn test_disk_header_hides_share() {
    let reader = disk_usage_reader(|sid| vec![usage_entry(sid, 110, 1_000, 10, 0, false)]);
    let view = pump(&reader);
    assert!(
        !view.usage_provenance.cache_account_complete(),
        "the call reported no account, so the summary says the session is incomplete"
    );
    assert_eq!(
        view.cache_read, None,
        "and an unreported zero is unknown rather than a measured miss"
    );
    assert_eq!(
        view.tokens_in,
        Some(1_000),
        "the count stands as a floor even though no call reported the field"
    );
}

/// The same path keeps a zero the endpoints did report: a session that reused
/// nothing and said so states a measured miss, share and all.
#[test]
fn test_disk_header_states_miss() {
    let reader = disk_usage_reader(|sid| vec![usage_entry(sid, 110, 1_000, 10, 0, true)]);
    let view = pump(&reader);
    assert!(
        view.usage_provenance.cache_account_complete(),
        "every call reported, so the session's account is whole"
    );
    assert_eq!(
        view.cache_read,
        Some(0),
        "a reported zero is a count the header states"
    );
    assert_eq!(
        view.usage_provenance.cache_hit_pct(0, 1_000),
        Some(0.0),
        "and the share over it is a measured miss"
    );
}

/// One model call's usage as a log entry, with the reporting flags the caller
/// asks for: the remaining flags stay off, as a log written before they existed
/// would have them.
fn usage_entry(
    sid: SessionId,
    ts: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    reported: bool,
) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts,
        prev_hash: None,
        event: SessionEvent::TurnUsage {
            turn: 1,
            call_in_turn: 1,
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cache_read,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            model: "test".into(),
            recovery: false,
            effort: None,
            input_reported: reported,
            cache_read_reported: reported,
            cache_write_reported: false,
        },
    }
}

/// A file-backed reader over the entries the builder writes for the session it
/// is handed, so a test states the log a header is read from.
fn disk_usage_reader(
    build: impl FnOnce(SessionId) -> Vec<SessionLogEntry>,
) -> SessionLogTrajectory {
    let root = temp_dir().join(format!("houyi_disk_usage_{}_{}", SessionId::new(), id()));
    create_dir_all(&root).expect("create temp root");
    let store = Arc::new(SessionStore::new(Box::new(LocalFileBackend::new(
        root.clone(),
    ))));
    let sid = SessionId::new();
    let rt = Runtime::new().expect("test runtime");
    for entry in build(sid) {
        rt.block_on(store.append(entry)).expect("append");
    }
    let (reader, _history) = reader_of(&store, sid);
    reader
}
