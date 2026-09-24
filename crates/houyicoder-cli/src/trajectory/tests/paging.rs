//! Tests for the loaded window: tail paging, the projection cache, and the
//! session-level totals that the window must not narrow.

use super::super::reader::{
    DELTA_MAX_BYTES, RESIDENT_PAGES, SessionLogTrajectory, TRAJECTORY_PAGE_TURNS,
};
use super::super::view::project;
use crate::session_history::SessionHistory;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_memory::LocalFileBackend;
use houyicoder_session::SessionStore;
use houyicoder_tui::view::trajectory_pane::{
    TrajectoryLog as _, TrajectoryRow, TrajectoryView, TrajectoryViewState,
};
use std::path::PathBuf;
use std::sync::Arc;

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
        first.title, "prompt 3",
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

/// Each turn row's number with the user input it names, in order.
fn turn_rows(view: &TrajectoryView) -> Vec<(usize, String)> {
    view.rows
        .iter()
        .filter_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some((turn.n, turn.title.clone())),
            TrajectoryRow::Bg(_) => None,
        })
        .collect()
}

/// The session turn numbers of a view's rows, in order.
fn turn_numbers(view: &TrajectoryView) -> Vec<usize> {
    view.rows
        .iter()
        .filter_map(|row| match row {
            TrajectoryRow::Turn(turn) => Some(turn.n),
            TrajectoryRow::Bg(_) => None,
        })
        .collect()
}

/// A reader over the given store, with the history reader it pages, so a test
/// can assert on what the reader asked the disk for.
pub(super) fn reader_of(
    store: &std::sync::Arc<houyicoder_session::SessionStore>,
    sid: SessionId,
) -> (SessionLogTrajectory, std::sync::Arc<SessionHistory>) {
    let log: std::sync::Arc<dyn houyicoder_api::session::SessionLog> = store.clone();
    let history = std::sync::Arc::new(SessionHistory::new(log.clone(), sid));
    (
        SessionLogTrajectory::with_history(history.clone(), log, sid, "test".into()),
        history,
    )
}

/// Pump the reader until its page lands. A draw never reads the log, so the
/// first frames after a page is asked for report that they are loading.
pub(super) fn pump(reader: &SessionLogTrajectory) -> Arc<TrajectoryView> {
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
    let store = Arc::new(SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let sid = SessionId::new();
    let (reader, _history) = reader_of(&store, sid);

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
    let store = Arc::new(SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let sid = SessionId::new();
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
    let (reader, _history) = reader_of(&store, sid);
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
pub(super) fn disk_reader(turns: usize) -> (Arc<SessionStore>, SessionLogTrajectory, SessionId) {
    let (store, reader, sid, _history, _root) = disk_reader_at(turns);
    (store, reader, sid)
}

/// The same reader, with the directory its log lives in, so a test can change
/// the bytes the reader will later read.
pub(super) fn disk_reader_at(
    turns: usize,
) -> (
    Arc<SessionStore>,
    SessionLogTrajectory,
    SessionId,
    Arc<SessionHistory>,
    PathBuf,
) {
    let root = std::env::temp_dir().join(format!(
        "houyi_disk_reader_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = Arc::new(SessionStore::new(Box::new(LocalFileBackend::new(
        root.clone(),
    ))));
    let sid = SessionId::new();
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
    let (reader, history) = reader_of(&store, sid);
    (store, reader, sid, history, root)
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
    let (_store, reader, _sid, history, _root) = disk_reader_at(150);
    let first = pump(&reader);
    assert_eq!(first.state, TrajectoryViewState::Ready);
    let (whole_after_load, reads_after_load) = {
        let (whole, reads, _) = history.read_stats();
        (whole, reads)
    };
    let mut previous = Arc::clone(&first);
    for _ in 0..1000 {
        let next = reader.trajectory();
        assert_eq!(next.state, TrajectoryViewState::Ready);
        assert!(
            Arc::ptr_eq(&previous, &next),
            "a settled window is served from the cache, not rebuilt"
        );
        previous = next;
    }
    let (whole, reads, _) = history.read_stats();
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
                TrajectoryRow::Turn(turn) => Some(turn.title.clone()),
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

/// The window slides rather than grows: walking back far enough drops the page
/// furthest from the walk, which is the newest one. The rows that remain keep
/// their session turn numbers, so a caller that recorded the turn it was on can
/// still find it after the page under it moved.
#[test]
fn test_evict_keeps_selection() {
    let (_store, reader, _sid) = disk_reader(500);
    let first = pump(&reader);
    let numbers = turn_numbers(&first);
    assert_eq!(
        numbers.first().copied(),
        Some(401),
        "the tail page opens at 401"
    );
    assert_eq!(numbers.last().copied(), Some(500));

    // The user is on the oldest resident turn when the walk back starts, so
    // that is the turn whose place must survive the page that arrives.
    let selected = numbers[0];
    reader.load_older();
    let wider = pump(&reader);
    let numbers = turn_numbers(&wider);
    assert!(
        numbers.contains(&selected),
        "the turn the user was on is still resident: {numbers:?}"
    );
    assert_eq!(
        numbers.iter().position(|n| *n == selected),
        Some(TRAJECTORY_PAGE_TURNS),
        "it moved by the page that arrived, not by a row count"
    );

    // The next walk starts from the oldest turn of the wider window, and that
    // walk is the one that slides: the newest page is dropped, and every number
    // that remains is unchanged.
    let selected = numbers[0];
    reader.load_older();
    let slid = pump(&reader);
    let numbers = turn_numbers(&slid);
    assert_eq!(numbers.first().copied(), Some(201));
    assert_eq!(
        numbers.last().copied(),
        Some(400),
        "the newest page was dropped"
    );
    assert!(!numbers.contains(&500), "the far end is the one that goes");
    assert!(
        numbers.contains(&selected),
        "the turn the user was on stays"
    );
}

/// Walking back through a long session keeps the window contiguous and bounded:
/// no gap, no repeated turn, and no more pages resident than the bound allows.
#[test]
fn test_scroll_beyond_two_pages() {
    let (_store, reader, _sid) = disk_reader(500);
    drop(pump(&reader));
    let mut previous_first = turn_numbers(&reader.trajectory())[0];
    for _ in 0..5 {
        reader.load_older();
        let view = pump(&reader);
        let numbers = turn_numbers(&view);
        assert!(
            !numbers.is_empty(),
            "the window never empties while walking back"
        );
        for window in numbers.windows(2) {
            assert_eq!(
                window[1],
                window[0] + 1,
                "no gap and no repeat: {numbers:?}"
            );
        }
        assert!(
            numbers.len() <= RESIDENT_PAGES * TRAJECTORY_PAGE_TURNS,
            "the resident window stays bounded: {}",
            numbers.len()
        );
        assert!(
            numbers[0] <= previous_first,
            "the walk only moves back: {} then {}",
            previous_first,
            numbers[0]
        );
        previous_first = numbers[0];
    }
    assert_eq!(
        previous_first, 1,
        "the walk reaches the session's first turn"
    );
}

/// Home replaces the window with the head of the log in one step, without
/// reading the whole log into it.
#[test]
fn test_home_jumps_to_earliest() {
    let (_store, reader, _sid) = disk_reader(500);
    let first = pump(&reader);
    assert_eq!(turn_numbers(&first)[0], 401, "the pane opens at the tail");

    reader.load_earliest();
    let head = pump(&reader);
    let numbers = turn_numbers(&head);
    assert_eq!(
        numbers.first().copied(),
        Some(1),
        "Home lands on the first turn"
    );
    assert_eq!(numbers.last().copied(), Some(TRAJECTORY_PAGE_TURNS));
    assert_eq!(head.hidden_turns, 0, "nothing sits before the head");
    assert_eq!(head.newer_hidden, 400, "and the rest is newer, not older");
}

/// End puts the tail back after the window has been walked away from it.
#[test]
fn test_end_returns_to_tail() {
    let (_store, reader, _sid) = disk_reader(500);
    drop(pump(&reader));
    reader.load_earliest();
    let head = pump(&reader);
    assert_eq!(head.newer_hidden, 400, "the window is at the head");

    reader.return_to_tail();
    let tail = pump(&reader);
    let numbers = turn_numbers(&tail);
    assert_eq!(
        numbers.last().copied(),
        Some(500),
        "End lands on the newest turn"
    );
    assert_eq!(tail.newer_hidden, 0, "nothing newer than the tail");
    assert_eq!(tail.hidden_turns, 400);
}

/// An append must not move a window the user walked back to: the new turn is
/// counted as newer, and the resident rows keep their place and their numbers.
#[test]
fn test_append_after_evict() {
    let (store, reader, sid) = disk_reader(500);
    drop(pump(&reader));
    for _ in 0..2 {
        reader.load_older();
        drop(pump(&reader));
    }
    let before = turn_numbers(&pump(&reader));
    assert_eq!(before.first().copied(), Some(201));
    assert!(!before.contains(&500), "the slide dropped the tail page");
    let newer_before = reader.trajectory().newer_hidden;
    assert!(newer_before > 0, "the window is behind the tail");

    futures::executor::block_on(store.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 999_000,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "prompt 500".into(),
        },
    }))
    .expect("append");

    let after = pump(&reader);
    assert_eq!(
        turn_numbers(&after),
        before,
        "an append does not move a window the user walked back to"
    );
    assert_eq!(
        after.newer_hidden,
        newer_before + 1,
        "the appended turn is counted as newer, not read"
    );
    assert_eq!(after.total_turns, 501);
}

/// An anchor whose bytes no longer describe the turn it named is refused, not
/// paged: the window is dropped and the tail read again, because nothing
/// resident can be trusted to abut a log whose offsets moved.
#[test]
fn test_stale_anchor_refused() {
    let (_store, reader, sid, _history, root) = disk_reader_at(150);
    let first = pump(&reader);
    assert_eq!(
        turn_numbers(&first)[0],
        51,
        "the tail page opens at turn 51"
    );

    // The bytes the anchor names are gone: the log is rewritten short, so the
    // offset the anchor holds no longer lands on its turn.
    let log = root.join(sid.to_string()).join("log.jsonl");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&log)
        .expect("open the log");
    file.set_len(1024).expect("truncate the log");

    reader.load_older();
    let after = pump(&reader);
    let numbers = turn_numbers(&after);
    assert!(
        numbers.first().copied().unwrap_or(0) > 100,
        "the refused anchor drops the window back to the tail: {numbers:?}"
    );
}

/// A clear starts a new history whose turn numbers begin again, so the view
/// says which history it shows: a caller compares that to tell whether a turn
/// number it holds still names the same turn.
#[test]
fn test_clear_moves_history_generation() {
    let (store, reader, sid) = disk_reader(5);
    let first = pump(&reader);
    let before = first.history_generation;

    store.reset_trajectory(sid);
    futures::executor::block_on(store.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 999_000,
        prev_hash: None,
        event: SessionEvent::ContextCleared { prior_turn: 5 },
    }))
    .expect("append the clear");

    let after = pump(&reader);
    assert!(
        after.history_generation > before,
        "a clear starts a new history: {before} then {}",
        after.history_generation
    );
}

/// A history whose start cannot be located leaves the window where it is:
/// Home must not replace a real window with an empty list.
/// A reader over a history whose start cannot be named: its first line is wider
/// than one range read, so the head read finds no event to name it with.
fn unlocatable_head_reader() -> SessionLogTrajectory {
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;

    let root = std::env::temp_dir().join(format!(
        "houyi_traj_head_unknown_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = std::sync::Arc::new(SessionStore::new(Box::new(LocalFileBackend::new(root))));
    let sid = SessionId::new();
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    rt.block_on(store.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::AssistantMessage {
            text: "x".repeat(1100 * 1024),
            thinking: None,
        },
    }))
    .expect("append the wide event");
    for i in 0..3u64 {
        rt.block_on(store.append(SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: 1000 + i * 1000,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: format!("prompt {i}"),
            },
        }))
        .expect("append");
    }
    let (reader, _history) = reader_of(&store, sid);
    reader
}

#[test]
fn test_unlocatable_head_keeps_window() {
    let reader = unlocatable_head_reader();
    let before = turn_numbers(&pump(&reader));
    assert!(!before.is_empty(), "the tail window is real");

    reader.load_earliest();
    let after = turn_numbers(&pump(&reader));
    assert_eq!(
        after, before,
        "an unlocatable head leaves the window as it was"
    );
}

/// A store whose log reads panic, so the reader's worker dies without sending a
/// page. Nothing else about the store changes.
fn panicking_reader() -> SessionLogTrajectory {
    use houyicoder_context::{
        CheckpointId, CheckpointManifest, ContextBackend, ContextError, ReverseRead, SessionId,
        SessionLogEntry,
    };

    /// The same future type the trait spells, without depending on the crate
    /// that defines its alias.
    type PFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

    struct PanicReads(houyicoder_memory::InMemoryBackend);

    impl ContextBackend for PanicReads {
        fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
            self.0.append(event)
        }

        fn read_range(
            &self,
            session: SessionId,
            from: Option<EventId>,
            to: Option<EventId>,
        ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            self.0.read_range(session, from, to)
        }

        fn replay(
            &self,
            session: SessionId,
        ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            self.0.replay(session)
        }

        fn write_checkpoint(
            &self,
            manifest: CheckpointManifest,
        ) -> PFut<'_, Result<CheckpointId, ContextError>> {
            self.0.write_checkpoint(manifest)
        }

        fn read_checkpoint(
            &self,
            id: CheckpointId,
        ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
            self.0.read_checkpoint(id)
        }

        fn list_checkpoints(
            &self,
            session: SessionId,
        ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
            self.0.list_checkpoints(session)
        }

        fn supports_log_windows(&self) -> bool {
            true
        }

        fn log_size(&self, _session: SessionId) -> u64 {
            // The walk only needs a non-zero size to start; the read it asks
            // for next is the one that fails.
            4096
        }

        fn read_lines_reverse(&self, _session: SessionId, _from: u64, _max: u64) -> ReverseRead {
            panic!("this log read is broken")
        }
    }

    // The log is written through the backend directly: the store's own append
    // reads the log back to link the chain, and every read panics here.
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    let sid = SessionId::new();
    let inner = houyicoder_memory::InMemoryBackend::new();
    for i in 0..3u64 {
        rt.block_on(ContextBackend::append(
            &inner,
            SessionLogEntry {
                id: EventId::new(),
                session: sid,
                ts: i * 1000,
                prev_hash: None,
                event: SessionEvent::UserInput {
                    text: format!("prompt {i}"),
                },
            },
        ))
        .expect("append");
    }
    let store = std::sync::Arc::new(houyicoder_session::SessionStore::new(Box::new(PanicReads(
        inner,
    ))));
    let (reader, _history) = reader_of(&store, sid);
    reader
}

/// A read whose worker dies is retried, and a run of them is reported: a pane
/// that kept serving the loading view of a read that never arrived would stay
/// on it for the rest of the session.
#[test]
fn test_broken_read_ends_failed() {
    let reader = panicking_reader();
    let mut seen = Vec::new();
    for _ in 0..80 {
        let view = reader.trajectory();
        if seen.last() != Some(&view.state) {
            seen.push(view.state);
        }
        if view.state == TrajectoryViewState::Failed {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        seen.contains(&TrajectoryViewState::Failed),
        "a broken read ends in a reported failure rather than loading forever: {seen:?}"
    );
}

/// A read serves the intent that dispatched it. Home during the first read
/// replaces it rather than waiting for the tail the user just walked away from.
#[test]
fn test_home_supersedes_first_read() {
    let (_store, reader, _sid) = disk_reader(500);
    // One draw dispatches the tail read and leaves it in flight.
    assert_eq!(reader.trajectory().state, TrajectoryViewState::Loading);
    reader.load_earliest();

    let head = pump(&reader);
    assert_eq!(
        turn_numbers(&head).first().copied(),
        Some(1),
        "Home's own read lands, not the tail it replaced"
    );
}

/// End during a head read replaces it: the user asked for the newest turns, and
/// a head page arriving later must not move the window back.
#[test]
fn test_end_supersedes_head_read() {
    let (_store, reader, _sid) = disk_reader(500);
    drop(pump(&reader));
    reader.load_earliest();
    reader.return_to_tail();

    let tail = pump(&reader);
    let numbers = turn_numbers(&tail);
    assert_eq!(
        numbers.last().copied(),
        Some(500),
        "the tail is what End asked for: {numbers:?}"
    );
    assert_eq!(tail.newer_hidden, 0, "and the window follows it");
}

/// End during an older read replaces it too: the older page must not be applied
/// onto the window End just put back.
#[test]
fn test_end_supersedes_older_read() {
    let (_store, reader, _sid) = disk_reader(500);
    drop(pump(&reader));
    reader.load_older();
    reader.return_to_tail();

    let tail = pump(&reader);
    let numbers = turn_numbers(&tail);
    assert_eq!(
        numbers.first().copied(),
        Some(401),
        "the tail page: {numbers:?}"
    );
    assert_eq!(numbers.last().copied(), Some(500));
}

/// A walk back during a tail refresh replaces the refresh rather than waiting
/// for it: the user's Up is not dropped because the session appended.
#[test]
fn test_older_supersedes_tail_refresh() {
    let (store, reader, sid) = disk_reader(500);
    drop(pump(&reader));
    futures::executor::block_on(store.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 999_000,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "prompt 500".into(),
        },
    }))
    .expect("append");

    // One draw dispatches the refresh the append called for, leaving it in
    // flight, and the walk back then asks for the older page.
    assert_eq!(reader.trajectory().state, TrajectoryViewState::LoadingOlder);
    reader.load_older();

    let view = pump(&reader);
    let rows = turn_rows(&view);
    assert!(
        rows.len() > TRAJECTORY_PAGE_TURNS && rows.first().unwrap().0 < 401,
        "the walk back loaded an older page instead of waiting for the refresh: {rows:?}"
    );
    for (n, input) in &rows {
        assert_eq!(
            input,
            &format!("prompt {}", n - 1),
            "every row still names its own turn: {rows:?}"
        );
    }
    assert_eq!(
        view.newer_hidden, 1,
        "the appended turn is newer than the window, not folded into it"
    );
}

/// An append while a page is in hand must not renumber it: the rows keep the
/// numbers their page was read with, and the turn that is not loaded is
/// reported as newer rather than taking the newest number.
#[test]
fn test_append_keeps_window_numbers() {
    let (store, reader, sid) = disk_reader(500);
    drop(pump(&reader));
    futures::executor::block_on(store.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 999_000,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "prompt 500".into(),
        },
    }))
    .expect("append");

    // The draw after the append asks for a refreshed page; the one in hand is
    // the old one until it lands.
    let stale = reader.trajectory();
    assert_eq!(stale.state, TrajectoryViewState::LoadingOlder);
    assert_eq!(
        stale.newer_hidden, 1,
        "the appended turn is newer, not renumbered into the window"
    );
    let last = turn_rows(&stale).pop().expect("a turn row");
    assert_eq!(
        last,
        (500, "prompt 499".to_string()),
        "the newest row keeps the number it was read with"
    );

    // Walking back keeps every number naming its own turn.
    reader.load_older();
    let view = pump(&reader);
    for (n, input) in turn_rows(&view) {
        assert_eq!(
            input,
            format!("prompt {}", n - 1),
            "row {n} names its own turn"
        );
    }
    assert_eq!(view.newer_hidden, 1);
}

/// A clear that lands before the next draw leaves the window describing the
/// history it ended. Walking back from it must drop that window rather than
/// page behind it under the new history's name.
#[test]
fn test_clear_drops_older_walk() {
    let (store, reader, sid) = disk_reader(150);
    let first = pump(&reader);
    assert!(
        first.hidden_turns > 0,
        "older turns exist to be walked back to"
    );

    // The clear lands with no draw in between, so the resident window is still
    // the one the previous history was read for.
    store.reset_trajectory(sid);
    futures::executor::block_on(store.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 999_000,
        prev_hash: None,
        event: SessionEvent::ContextCleared { prior_turn: 150 },
    }))
    .expect("append the clear");
    reader.load_older();

    for _ in 0..200 {
        let view = reader.trajectory();
        let prompts: Vec<String> = view
            .rows
            .iter()
            .filter_map(|row| match row {
                TrajectoryRow::Turn(turn) => Some(turn.title.clone()),
                TrajectoryRow::Bg(_) => None,
            })
            .collect();
        assert!(
            !prompts.iter().any(|p| p.starts_with("prompt ")),
            "a cleared history's turns are never shown: {prompts:?}"
        );
        if view.state == TrajectoryViewState::Ready {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// A held key repeats, and a repeat asks for the same end again. Treating the
/// repeat as a new intent would replace a read that never gets to finish, so a
/// burst of one intent reads the log once rather than once per repeat.
#[test]
fn test_repeated_intent_reads_once() {
    let (_store, reader, _sid, history, _root) = disk_reader_at(500);
    drop(pump(&reader));
    let (_, reads_before, _) = history.read_stats();
    for _ in 0..100 {
        reader.load_earliest();
    }
    let view = pump(&reader);
    assert_eq!(view.state, TrajectoryViewState::Ready, "the head lands");
    // A superseded worker stops at its next chunk; give the burst's strays a
    // moment to do that before counting.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let (_, reads_after, _) = history.read_stats();
    assert!(
        reads_after - reads_before < 20,
        "a hundred repeats of one intent read the log once, not once per repeat: {}",
        reads_after - reads_before
    );
}

/// Home before the first page lands, on a history whose start cannot be named:
/// the pane falls back to the tail rather than dispatching a page it drops on
/// every frame, which would leave it loading forever.
#[test]
fn test_home_before_first_page() {
    let reader = unlocatable_head_reader();
    reader.load_earliest();

    let view = pump(&reader);
    assert_eq!(view.state, TrajectoryViewState::Ready, "the pane settles");
    assert!(
        !view.rows.is_empty(),
        "and the tail is what it has to show: {:?}",
        turn_numbers(&view)
    );
}

/// An append is read as a delta: the window takes what the log added after the
/// byte it ends at, rather than reading a page again for one event.
#[test]
fn test_append_reads_delta() {
    let (store, reader, sid, history, _root) = disk_reader_at(300);
    let before = turn_rows(&pump(&reader));
    let (_, reads_before, bytes_before) = history.read_stats();

    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    rt.block_on(store.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 900_000,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "appended".into(),
        },
    }))
    .expect("append");
    let after = turn_rows(&pump(&reader));
    let (_, reads_after, bytes_after) = history.read_stats();

    assert_eq!(
        reads_after - reads_before,
        1,
        "one read answers the append, not a page walk"
    );
    // The delta asks for a step of the walk it replaces, so the append costs
    // the window's own end rather than a page read backwards from EOF.
    assert!(
        bytes_after - bytes_before <= DELTA_MAX_BYTES,
        "and it asks for the delta budget rather than a page: {} bytes",
        bytes_after - bytes_before
    );
    assert_eq!(
        after.len(),
        before.len() + 1,
        "the window gains the new turn"
    );
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "and the resident rows keep the numbers and titles they had"
    );
}

/// A burst bigger than the delta budget is read as the tail: a resumed session
/// or a wide tool result is not an append the window can extend itself with.
#[test]
fn test_burst_reads_tail() {
    let (store, reader, sid, history, _root) = disk_reader_at(20);
    drop(pump(&reader));
    let (_, _, bytes_before) = history.read_stats();

    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    rt.block_on(store.append(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 900_000,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "y".repeat((DELTA_MAX_BYTES + 4096) as usize),
        },
    }))
    .expect("append a burst");
    let view = pump(&reader);
    let (_, _, bytes_after) = history.read_stats();

    assert!(
        bytes_after - bytes_before > DELTA_MAX_BYTES,
        "the read is a page, not the delta budget: {} bytes",
        bytes_after - bytes_before
    );
    assert!(
        turn_rows(&view)
            .iter()
            .any(|(_, title)| title.starts_with("yyy")),
        "and the burst is in the window"
    );
}

/// An append that lands while a delta is in flight is read by the next delta
/// rather than folded into the first: the read is bounded by the byte it was
/// dispatched for, so the window never holds bytes its own watermark does not
/// describe, and no frame falls back to walking the tail page again.
#[test]
fn test_append_delta_stays_bounded() {
    // The first delta read is held open, so the second append certainly lands
    // while it is in flight rather than racing it.
    let (store, reader, sid, history, entered, release) = gated_disk_reader(300, "append");
    drop(pump(&reader));

    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    let append = |text: &str, ts: u64| {
        let entry = SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts,
            prev_hash: None,
            event: SessionEvent::UserInput { text: text.into() },
        };
        rt.block_on(store.append(entry)).expect("append");
    };

    let (_, reads_before, bytes_before) = history.read_stats();
    append("first", 900_000);
    drop(reader.trajectory());
    entered
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the delta read starts");
    append("second", 901_000);
    release.send(()).ok();

    let view = pump(&reader);
    let (_, reads_after, bytes_after) = history.read_stats();

    let titles: Vec<String> = turn_rows(&view)
        .into_iter()
        .map(|(_, title)| title)
        .collect();
    assert_eq!(
        titles.iter().filter(|title| *title == "first").count(),
        1,
        "the first append is in the window once: {titles:?}"
    );
    assert_eq!(
        titles.iter().filter(|title| *title == "second").count(),
        1,
        "and so is the one that landed while the read was in flight"
    );
    assert_eq!(reads_after - reads_before, 2, "one range read per append");
    // A delta that ran past the byte it was dispatched for would leave the
    // window's end at EOF with a watermark that describes less, and the next
    // frame would read a whole page behind it.
    assert!(
        bytes_after - bytes_before < DELTA_MAX_BYTES,
        "and both are deltas rather than a page read behind them: {} bytes",
        bytes_after - bytes_before
    );
}

/// A range read never returns bytes past the count it was asked for, which is
/// what lets a delta be applied under the watermark it was dispatched with.
#[test]
fn test_range_read_bounded() {
    let (store, _reader, sid, history, _root) = disk_reader_at(50);
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    for i in 0..20u64 {
        rt.block_on(store.append(SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: 1_000_000 + i,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: format!("later {i}"),
            },
        }))
        .expect("append");
    }
    let size = history.log_size();
    assert!(size > 0, "the log has bytes");

    for budget in [1u64, 64, 512, 4096] {
        let window = history.window(0, budget);
        for event in &window.events {
            assert!(
                event.byte_offset < budget,
                "an event at {} was returned for a {budget}-byte read",
                event.byte_offset
            );
        }
        assert!(
            window.next_offset <= budget,
            "the read stopped at {} for a {budget}-byte budget",
            window.next_offset
        );
    }
}

/// A file-backed reader whose first range read waits for the test, so an append
/// can land while a delta is in flight.
fn gated_disk_reader(
    turns: usize,
    tag: &str,
) -> (
    Arc<SessionStore>,
    SessionLogTrajectory,
    SessionId,
    Arc<SessionHistory>,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    use houyicoder_context::{
        CheckpointId, CheckpointManifest, ContextBackend, ContextError, LogRangeRead, ReverseRead,
    };

    type PFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

    struct Gated {
        inner: LocalFileBackend,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        held: std::sync::atomic::AtomicBool,
    }

    impl ContextBackend for Gated {
        fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
            self.inner.append(event)
        }
        fn read_range(
            &self,
            session: SessionId,
            from: Option<EventId>,
            to: Option<EventId>,
        ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            self.inner.read_range(session, from, to)
        }
        fn replay(
            &self,
            session: SessionId,
        ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            self.inner.replay(session)
        }
        fn write_checkpoint(
            &self,
            manifest: CheckpointManifest,
        ) -> PFut<'_, Result<CheckpointId, ContextError>> {
            self.inner.write_checkpoint(manifest)
        }
        fn read_checkpoint(
            &self,
            id: CheckpointId,
        ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
            self.inner.read_checkpoint(id)
        }
        fn list_checkpoints(
            &self,
            session: SessionId,
        ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
            self.inner.list_checkpoints(session)
        }
        fn supports_log_windows(&self) -> bool {
            true
        }
        fn log_size(&self, session: SessionId) -> u64 {
            self.inner.log_size(session)
        }
        fn read_lines_reverse(&self, session: SessionId, from: u64, max: u64) -> ReverseRead {
            self.inner.read_lines_reverse(session, from, max)
        }
        fn read_log_range(&self, session: SessionId, from: u64, max: u64) -> LogRangeRead {
            if !self.held.swap(true, std::sync::atomic::Ordering::SeqCst) {
                self.entered.send(()).ok();
                self.release.lock().expect("release lock").recv().ok();
            }
            self.inner.read_log_range(session, from, max)
        }
    }

    let root = std::env::temp_dir().join(format!(
        "houyi_gate_{tag}_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let store = Arc::new(SessionStore::new(Box::new(Gated {
        inner: LocalFileBackend::new(root),
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
        held: std::sync::atomic::AtomicBool::new(false),
    })));
    let sid = SessionId::new();
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    for i in 0..turns as u64 {
        rt.block_on(store.append(SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: i * 1000,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: format!("prompt {i}"),
            },
        }))
        .expect("append");
    }
    let (reader, history) = reader_of(&store, sid);
    (store, reader, sid, history, entered_rx, release_tx)
}
