//! Window-slide and selection tests for the trajectory pane: what happens to
//! the loaded window and to the turn the user is on when a page arrives.

use super::super::list;
use super::super::*;
use crate::view::working;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A window of turn rows numbered first..=last, over a session of total turns.
fn window_view(first: usize, last: usize, total: usize, generation: u64) -> TrajectoryView {
    let rows = (first..=last)
        .map(|n| {
            TrajectoryRow::Turn(TrajectoryTurn {
                boundary_before: Vec::new(),
                n,
                user_input: format!("prompt {n}"),
                tokens_in: None,
                tokens_out: None,
                cache_read: None,
                cache_write: None,
                models: Vec::new(),
                efforts: Vec::new(),
                reasoning_tokens: None,
                tool_count: 0,
                tool_fail: 0,
                retries: 0,
                duration_ms: 0,
                success: true,
                records: Vec::new(),
            })
        })
        .collect();
    TrajectoryView {
        state: TrajectoryViewState::Ready,
        skipped_records: 0,
        models_used: 1,
        tool_calls: 0,
        session_id: "scripted".into(),
        model: "scripted".into(),
        total_turns: total,
        tokens_in: None,
        tokens_out: None,
        cache_read: None,
        failures: 0,
        duration_secs: 0,
        timing: SessionTiming::default(),
        hidden_turns: first.saturating_sub(1),
        newer_hidden: total.saturating_sub(last),
        history_generation: generation,
        subagent_usage: None,
        rows,
    }
}

/// A TrajectoryLog that serves a scripted sequence of views and counts the
/// window moves it was asked for, so a test can drive the cursor across a
/// window change without a disk or a worker.
struct ScriptedLog {
    views: Mutex<VecDeque<TrajectoryView>>,
    earliest: AtomicUsize,
    older: AtomicUsize,
    tail: AtomicUsize,
}

impl ScriptedLog {
    fn new(views: Vec<TrajectoryView>) -> Self {
        Self {
            views: Mutex::new(views.into()),
            earliest: AtomicUsize::new(0),
            older: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Move to the next scripted view; the last one is served from then on.
    fn advance(&self) {
        let mut views = self.views.lock().unwrap();
        if views.len() > 1 {
            views.pop_front();
        }
    }
}

impl TrajectoryLog for ScriptedLog {
    fn trajectory(&self) -> Arc<TrajectoryView> {
        let views = self.views.lock().unwrap();
        Arc::new(
            views
                .front()
                .cloned()
                .unwrap_or_else(|| window_view(1, 0, 0, 1)),
        )
    }

    fn load_older(&self) {
        self.older.fetch_add(1, Ordering::Relaxed);
    }

    fn load_earliest(&self) {
        self.earliest.fetch_add(1, Ordering::Relaxed);
    }

    fn return_to_tail(&self) {
        self.tail.fetch_add(1, Ordering::Relaxed);
    }
}

/// The cursor is a position in a window that slides, so it cannot be the
/// selection: after a page arrives the same index names a different turn. The
/// selection is the turn number, and the row is found from it again.
#[test]
fn test_cursor_restored_by_turn() {
    let log = Arc::new(ScriptedLog::new(vec![
        window_view(401, 500, 500, 1),
        window_view(301, 500, 500, 1),
    ]));
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory_log = Some(log.clone());
    let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();

    // The user is on turn 401 when the walk back starts.
    app.trajectory.set_cursor(0);
    app.trajectory.select_turn(401, 1);
    terminal
        .draw(|f| {
            working::draw(f, &app);
        })
        .unwrap();
    assert_eq!(app.trajectory.cursor(), 0, "401 is the first row");

    // The older page lands, growing the window behind the selection.
    log.advance();
    terminal
        .draw(|f| {
            working::draw(f, &app);
        })
        .unwrap();
    assert_eq!(
        app.trajectory.cursor(),
        100,
        "the cursor follows the turn, not the row index"
    );
    let view = log.trajectory();
    match &view.rows[app.trajectory.cursor()] {
        TrajectoryRow::Turn(turn) => assert_eq!(turn.n, 401, "still the same turn"),
        _ => panic!("the cursor sits on a turn row"),
    }
}

/// Home and End move the window, not just the cursor: the first and last rows
/// of a sliding window are not the session's first and last turns.
#[test]
fn test_home_end_move_window() {
    let log = Arc::new(ScriptedLog::new(vec![window_view(401, 500, 500, 1)]));
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory_log = Some(log.clone());
    let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
    terminal
        .draw(|f| {
            working::draw(f, &app);
        })
        .unwrap();

    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(
        log.earliest.load(Ordering::Relaxed),
        1,
        "Home reads the head"
    );
    assert_eq!(app.trajectory.selected_turn(), 1);
    assert_eq!(app.trajectory.cursor(), 0);

    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(log.tail.load(Ordering::Relaxed), 1, "End reads the tail");
    assert_eq!(
        app.trajectory.selected_turn(),
        500,
        "End selects the session's newest turn"
    );
}

/// The header says which ends of the session are not loaded. A window walked
/// back from the tail hides newer turns, not only older ones, and a header
/// that named only the older count would leave the newest turns unaccounted.
#[test]
fn test_header_reports_newer() {
    let view = window_view(301, 400, 500, 1);
    let (header, _, _, _) = list::draw_turn_list(&view, 0, ratatui::layout::Rect::ZERO);
    let text: String = header
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
        .collect();
    assert!(text.contains("300 older not loaded"), "{text}");
    assert!(text.contains("100 newer not loaded"), "{text}");
    // With nothing hidden the label is just the count, as before.
    let whole = window_view(1, 20, 20, 1);
    let (header, _, _, _) = list::draw_turn_list(&whole, 0, ratatui::layout::Rect::ZERO);
    let text: String = header
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
        .collect();
    assert!(text.contains("20 turns"), "{text}");
    assert!(!text.contains("not loaded"), "{text}");
}

/// At L1 the L0 selection is not the pane's business: a move there must not
/// rewrite which turn the turn list will return to.
#[test]
fn test_level1_move_keeps_selection() {
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory.set_level(1);
    app.trajectory.select_turn(7, 1);
    app.trajectory.set_list_len(3);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        app.trajectory.selected_turn(),
        7,
        "an L1 move is not the L0 selection"
    );
}

/// A clear starts a new history whose turn numbers begin again, so a selection
/// from the old one must not be restored onto a turn it does not name.
#[test]
fn test_selection_dropped_across_history() {
    let log = Arc::new(ScriptedLog::new(vec![
        window_view(1, 100, 100, 1),
        window_view(1, 100, 100, 2),
    ]));
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory_log = Some(log.clone());
    let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();

    // The user selected turn 80 of the first history.
    app.trajectory.select_turn(80, 1);
    terminal
        .draw(|f| {
            working::draw(f, &app);
        })
        .unwrap();
    assert_eq!(app.trajectory.cursor(), 79, "the row of turn 80");

    // The pane reads a window of the same numbers, but of a new history, and
    // the cursor sits where the user left it.
    app.trajectory.set_cursor(0);
    log.advance();
    terminal
        .draw(|f| {
            working::draw(f, &app);
        })
        .unwrap();
    assert_eq!(
        app.trajectory.selected_turn(),
        0,
        "a selection from another history is dropped"
    );
    assert_eq!(
        app.trajectory.cursor(),
        0,
        "and it does not drag the cursor to a turn it does not name"
    );
}

/// At the record levels Home and End stay the ends of the list in hand: moving
/// the window there would re-point the drilled turn under the drill, and at the
/// event detail there is no list to move in at all.
#[test]
fn test_home_end_stay_local() {
    let log = Arc::new(ScriptedLog::new(vec![window_view(401, 500, 500, 1)]));
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory_log = Some(log.clone());
    app.trajectory.set_level(1);
    app.trajectory.set_list_len(4);

    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(app.trajectory.cursor(), 3, "End is the last record");
    assert_eq!(
        log.tail.load(Ordering::Relaxed),
        0,
        "and the window did not move"
    );

    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(app.trajectory.cursor(), 0, "Home is the first record");
    assert_eq!(
        log.earliest.load(Ordering::Relaxed),
        0,
        "and the window did not move"
    );

    // The event detail holds one record: there is no list to move in, and the
    // window is not its to move either.
    app.trajectory.set_level(2);
    app.trajectory.set_cursor(0);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(app.trajectory.cursor(), 0, "an event detail does not move");
    assert_eq!(log.tail.load(Ordering::Relaxed), 0);
    assert_eq!(log.earliest.load(Ordering::Relaxed), 0);
}

/// Leaving the turn detail returns to the turn the drill started from, not to
/// the first row: the drill is a look at one turn, and stepping back must not
/// move the user elsewhere in the history.
#[test]
fn test_esc_restores_turn() {
    let log = Arc::new(ScriptedLog::new(vec![window_view(401, 500, 500, 1)]));
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory_log = Some(log.clone());
    let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
    terminal
        .draw(|f| {
            working::draw(f, &app);
        })
        .unwrap();

    // The user walks to turn 405 and opens it.
    for _ in 0..4 {
        crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    assert_eq!(app.trajectory.cursor(), 4);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.trajectory.level(), 1);
    assert_eq!(
        app.trajectory.selected_turn(),
        405,
        "the drill is about 405"
    );

    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.trajectory.level(), 0, "Esc returns to the turn list");
    assert_eq!(
        app.trajectory.cursor(),
        4,
        "and the cursor is back on the turn it left"
    );
    assert_eq!(app.trajectory.selected_turn(), 405);
}

/// Leaving the event detail returns to the record list with the record still
/// selected: the user was looking at one record, not at the top of the list.
#[test]
fn test_esc_keeps_record() {
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory.set_level(2);
    app.trajectory.set_cursor(3);

    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.trajectory.level(), 1, "Esc returns to the record list");
    assert_eq!(app.trajectory.cursor(), 3, "with the record still selected");
}

/// Up at the top of the record list stays there. Only the turn list widens its
/// window, because loading older history while the user is reading one turn
/// would move that turn out from under the drill.
#[test]
fn test_level1_up_keeps_window() {
    let log = Arc::new(ScriptedLog::new(vec![window_view(401, 500, 500, 1)]));
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory_log = Some(log.clone());
    app.trajectory.set_level(1);
    app.trajectory.set_list_len(4);
    app.trajectory.set_cursor(0);

    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.trajectory.cursor(), 0, "the top of the record list");
    assert_eq!(
        log.older.load(Ordering::Relaxed),
        0,
        "and the window did not load behind the drill"
    );

    // At the turn list the same key does widen the window.
    app.trajectory.set_level(0);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        log.older.load(Ordering::Relaxed),
        1,
        "the turn list loads the older page"
    );
}
