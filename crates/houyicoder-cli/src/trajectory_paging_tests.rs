//! Tests for the loaded window: tail paging, the projection cache, and the
//! session-level totals that the window must not narrow.

use super::*;
use crate::trajectory_reader::{SessionLogTrajectory, TRAJECTORY_PAGE_TURNS};
use houyicoder_context::{EventId, SessionLogEntry};
use houyicoder_tui::view::trajectory_pane::TrajectoryLog as _;

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

/// Pump the reader until its page lands. A draw never reads the log, so the
/// first frames after a page is asked for report that they are loading.
fn pump(reader: &SessionLogTrajectory) -> std::sync::Arc<TrajectoryView> {
    for _ in 0..400 {
        let view = reader.trajectory();
        if view.state == TrajectoryViewState::Ready {
            return view;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the page never landed");
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
    let first = pump(&reader);
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

    let after = pump(&reader);
    assert_eq!(after.total_turns, 1, "the append is picked up");
    let again = reader.trajectory();
    assert_eq!(
        again.total_turns, 1,
        "a second read of an unchanged log gives the same view"
    );
    assert_eq!(
        again.state,
        TrajectoryViewState::Ready,
        "and it is served from the page already in hand"
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
    let first = pump(&reader);
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
    let wider = pump(&reader);
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
fn test_subagent_usage_summed() {
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
        ev(
            110,
            SessionEvent::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 1_000,
                output_tokens: 100,
                cache_read_input_tokens: 900,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "test".into(),
                recovery: false,
                effort: None,
            },
        ),
        child("child-1", 180_000, 4_000, 170_000),
        child("child-2", 32_000, 1_000, 30_000),
    ];
    let view = project(&events, "test", 0);
    let delegated = view
        .subagent_usage
        .expect("two returns produce a delegated row");
    assert_eq!(delegated.calls, 2);
    assert_eq!(delegated.input, 212_000, "the children's input is summed");
    assert_eq!(delegated.output, 5_000);
    assert_eq!(delegated.cache_read, 200_000);
    assert_eq!(
        delegated.cache_hit_pct(),
        Some(200_000.0 / 212_000.0 * 100.0)
    );
    // The session totals fold the parent's calls and the children's, so the
    // headline reports what the whole session spent.
    assert_eq!(
        view.tokens_in,
        Some(213_000),
        "the session's economic account includes the delegated work"
    );
    assert_eq!(view.tokens_out, Some(5_100));
    assert_eq!(view.cache_read, Some(200_900));
}

/// A turn that reported no usage leaves the session total unknown: a child's
/// tokens are the child's own spend and cannot fill the parent's hole.
#[test]
fn test_subagent_usage_parent_unknown() {
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
            500,
            SessionEvent::SubagentReturn {
                child_session_id: "child-1".into(),
                status: "completed".into(),
                summary: "done".into(),
                result_ref: "child-1".into(),
                input_tokens: 180_000,
                output_tokens: 4_000,
                cache_read_input_tokens: 170_000,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
            },
        ),
    ];
    let view = project(&events, "test", 0);
    assert!(view.subagent_usage.is_some(), "the child is still reported");
    assert_eq!(
        view.tokens_in, None,
        "the parent's turn reported nothing, so the total stays unknown"
    );
}

/// A delegation that reported no usage of its own also leaves the total unknown.
#[test]
fn test_subagent_usage_unmeasured_unknown() {
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
            SessionEvent::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 1_000,
                output_tokens: 100,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "test".into(),
                recovery: false,
                effort: None,
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
    assert!(
        view.subagent_usage.is_some(),
        "the delegation is reported even without usage"
    );
    assert_eq!(
        view.tokens_in, None,
        "an unmeasured child leaves the session total unknown"
    );
}

/// A session with no delegation reports no delegated usage at all.
#[test]
fn test_subagent_usage_absent() {
    let events = vec![ev(
        100,
        SessionEvent::UserInput {
            text: "just talk".into(),
        },
    )];
    let view = project(&events, "test", 0);
    assert!(view.subagent_usage.is_none(), "no children, no row");
}

/// A child that reported no usage leaves the row without a cache share rather
/// than a fabricated one.
#[test]
fn test_subagent_usage_unknown() {
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
    let delegated = view.subagent_usage.expect("the call is still reported");
    assert_eq!(delegated.calls, 1);
    assert_eq!(delegated.input, 0);
    assert_eq!(
        delegated.cache_hit_pct(),
        None,
        "an unknown share is absent, not zero"
    );
}

/// A file-backed reader, which is the path the product runs on. The
/// in-memory store takes the mirror branch, so a bug in the page path would
/// not show up there.
fn disk_reader(
    turns: usize,
) -> (
    std::sync::Arc<houyicoder_session::SessionStore>,
    SessionLogTrajectory,
    houyicoder_context::SessionId,
) {
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;

    let root = std::env::temp_dir().join(format!(
        "houyi_disk_reader_{}_{}",
        houyicoder_context::SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = std::sync::Arc::new(SessionStore::new(Box::new(LocalFileBackend::new(root))));
    let sid = houyicoder_context::SessionId::new();
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    for i in 0..turns {
        rt.block_on(store.append(SessionLogEntry {
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
    (store, reader, sid)
}

/// A page read off the disk is refreshed when the session appends: the pane
/// stays open across turns, so a reader that kept its first page would never
/// show anything new.
#[test]
fn test_disk_reader_refreshes_tail() {
    let (store, reader, sid) = disk_reader(3);
    let first = pump(&reader);
    assert_eq!(first.rows.len(), 3);
    futures::executor::block_on(store.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 9000,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "prompt 3".into(),
        },
    }))
    .expect("append");
    let after = pump(&reader);
    assert_eq!(
        after.rows.len(),
        4,
        "the appended turn appears once the page is re-read"
    );
    assert_eq!(after.total_turns, 4);
}

/// The header reports the session, not the page: a page holds the newest
/// turns, and its own totals would report the page as the session.
#[test]
fn test_disk_header_uses_summary() {
    let (_store, reader, _sid) = disk_reader(150);
    let view = pump(&reader);
    assert_eq!(
        view.total_turns, 150,
        "the session's turn count, not the page's"
    );
    assert_eq!(
        view.hidden_turns, 50,
        "and the turns before the page are counted as hidden"
    );
}

/// Numbering continues from where the hidden turns left off, so the oldest
/// visible turn keeps the number it has in the session.
#[test]
fn test_disk_turn_numbers_continue() {
    let (_store, reader, _sid) = disk_reader(150);
    let view = pump(&reader);
    let first = view
        .rows
        .iter()
        .find_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.n),
            TrajectoryRow::Bg(_) => None,
        })
        .expect("a turn row");
    assert_eq!(first, 51, "the page starts at the session's 51st turn");
}

/// A settled window costs nothing to draw: the pane draws every frame, so a
/// frame that re-read the log or re-projected the page would put both on the
/// draw path.
#[test]
fn test_draw_reads_nothing() {
    let (_store, reader, _sid) = disk_reader(150);
    let first = pump(&reader);
    assert_eq!(first.state, TrajectoryViewState::Ready);
    let (whole_after_load, reads_after_load) = {
        let (whole, reads, _) = reader.history().read_stats();
        (whole, reads)
    };
    let mut previous = std::sync::Arc::clone(&first);
    for _ in 0..1000 {
        let next = reader.trajectory();
        assert_eq!(next.state, TrajectoryViewState::Ready);
        assert!(
            std::sync::Arc::ptr_eq(&previous, &next),
            "a settled window is served from the cache, not rebuilt"
        );
        previous = next;
    }
    let (whole, reads, _) = reader.history().read_stats();
    assert_eq!(whole, whole_after_load, "no draw read the log whole");
    assert_eq!(reads, reads_after_load, "and none read the log at all");
}

/// Asking for older history keeps the newest turns: the window widens behind
/// the tail rather than being replaced by the older page.
#[test]
fn test_disk_older_keeps_tail() {
    let (_store, reader, _sid) = disk_reader(150);
    let first = pump(&reader);
    let newest = first
        .rows
        .iter()
        .filter_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.n),
            TrajectoryRow::Bg(_) => None,
        })
        .max()
        .expect("turn rows");
    assert_eq!(newest, 150);

    reader.load_older();
    let wider = pump(&reader);
    let numbers: Vec<usize> = wider
        .rows
        .iter()
        .filter_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.n),
            TrajectoryRow::Bg(_) => None,
        })
        .collect();
    assert!(
        numbers.contains(&150),
        "the newest turn is still in the window: {numbers:?}"
    );
    assert_eq!(
        numbers.first().copied(),
        Some(1),
        "and the older page widened it rather than replacing it"
    );
}

/// A clear starts a new epoch. A page read in the old one describes turns the
/// session no longer counts, so it is dropped rather than shown and corrected
/// a frame later — and the rows already resident from that epoch go with it.
#[test]
fn test_clear_drops_old_epoch() {
    // More than one page, so the tail really has older turns behind it and
    // load_older dispatches a read instead of returning at the log's start.
    let (store, reader, sid) = disk_reader(150);
    let first = pump(&reader);
    assert_eq!(first.rows.len(), TRAJECTORY_PAGE_TURNS);
    assert!(first.hidden_turns > 0, "older turns exist to be read");

    // An older read is genuinely in flight when the clear lands. A clear is a
    // mirror reset followed by the boundary event, which is what makes the
    // boundary the new epoch's first durable event.
    reader.load_older();
    store.reset_trajectory(sid);
    futures::executor::block_on(store.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 999_000,
        prev_hash: None,
        event: SessionEvent::ContextCleared { prior_turn: 150 },
    }))
    .expect("append the clear");

    // Whatever the pump observes, the cleared epoch's turns must never appear,
    // and the new epoch's page must actually land: a pane that stayed on
    // loading would satisfy "never shows the old turns" while showing nothing.
    let mut settled = false;
    for _ in 0..200 {
        let view = reader.trajectory();
        let prompts: Vec<String> = view
            .rows
            .iter()
            .filter_map(|row| match row {
                TrajectoryRow::Turn(turn) => Some(turn.user_input.clone()),
                TrajectoryRow::Bg(_) => None,
            })
            .collect();
        assert!(
            !prompts.iter().any(|p| p.starts_with("prompt ")),
            "a cleared epoch's turns are never shown: {prompts:?}"
        );
        if view.state == TrajectoryViewState::Ready {
            settled = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        settled,
        "the new epoch's page lands rather than loading forever"
    );
}
