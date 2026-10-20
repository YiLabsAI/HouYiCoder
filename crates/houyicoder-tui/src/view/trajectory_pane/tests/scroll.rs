//! Tests for where the record level's body sits: the reader scrolls it, so
//! what the pane draws is the part they scrolled to.

use super::super::*;
use super::fixtures::{detail_of_all, record_of, turn, view_of};
use crate::composition::app;
use crate::keys::handle_working;
use crate::state::Pane;
use crate::test_harness::dump_buffer;
use crate::view::working;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// A trajectory log serving one turn whose record carries a body longer than
/// the pane, so a test can read which of its lines are on screen.
struct LongBodyLog {
    view: TrajectoryView,
    detail: TrajectoryDetailView,
}

impl TrajectoryLog for LongBodyLog {
    fn trajectory(&self) -> Arc<TrajectoryView> {
        Arc::new(self.view.clone())
    }

    fn load_older(&self) {}
    fn load_earliest(&self) {}
    fn return_to_tail(&self) {}
    fn request_detail(&self, _drill: &TrajectoryDrill) {}

    fn detail(&self, _key: &TrajectoryTurnKey) -> Arc<TrajectoryDetailView> {
        Arc::new(self.detail.clone())
    }
}

/// The body lines the pane is showing, each as the screen row it sits on and
/// its number, read off the rendered screen rather than off the pane's own
/// offset: which part of the body is on screen, and how far it moved, is what
/// a scroll either did or did not do.
fn body_lines_on_screen(app: &App) -> Vec<(usize, usize)> {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test backend");
    terminal
        .draw(|f| working::draw(f, app))
        .expect("draw the working screen");
    let screen = dump_buffer(terminal.backend().buffer());
    screen
        .lines()
        .enumerate()
        .filter_map(|(row, text)| {
            let at = text.find("body-")?;
            let number = text[at + "body-".len()..]
                .split_whitespace()
                .next()?
                .parse::<usize>()
                .ok()?;
            Some((row, number))
        })
        .collect()
}

/// The numbers of those lines, for a test that reads the window itself.
fn visible_body_lines(app: &App) -> Vec<usize> {
    body_lines_on_screen(app)
        .into_iter()
        .map(|(_, number)| number)
        .collect()
}

/// The pane's row for a given fragment, or None when it is not on screen.
fn shown(app: &App, needle: &str) -> bool {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test backend");
    terminal
        .draw(|f| working::draw(f, app))
        .expect("draw the working screen");
    dump_buffer(terminal.backend().buffer()).contains(needle)
}

/// An app on the record level, its record carrying forty numbered lines.
fn record_app() -> App {
    let mut app = app();
    app.ui.navigation.pane = Pane::Trajectory;
    let body = (1..=40)
        .map(|n| format!("body-{n:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut record = record_of(TrajectoryRecordKind::Model, None);
    record.output = Some(body);
    app.sessions.persistence.trajectory_log = Some(Arc::new(LongBodyLog {
        view: view_of(vec![turn(1, "ask")]),
        detail: detail_of_all(vec![record]),
    }));
    app
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Press a key the way the loop does: the frame drawn before it is what the
/// key acts on, because the frame is what tells the pane how far its body can
/// scroll.
fn press(app: &mut App, code: KeyCode) {
    visible_body_lines(app);
    handle_working(app, key(code));
}

/// Drill the record level the way the keys do, and leave the app there.
fn open_record(app: &mut App) {
    // The turn list must have been rendered once before Enter drills: the
    // guard reads the stashed row count.
    press(app, KeyCode::Enter);
    press(app, KeyCode::Enter);
}

/// The record body pages ten lines at a time: a page is a fixed ten lines, the
/// step the sibling detail panes move by, rather than the height of the pane
/// it happens to be drawn in.
#[test]
fn test_record_body_pages() {
    let mut app = record_app();
    open_record(&mut app);
    assert_eq!(app.ui.trajectory.level(), 2);
    let top = body_lines_on_screen(&app);
    assert_eq!(
        top.first().map(|(_, n)| *n),
        Some(1),
        "the level opens at the top: {top:?}"
    );

    // A page moves the body ten lines, so the line at the foot of the first
    // window sits ten rows higher while staying on screen.
    let foot = *top.last().expect("a body line on screen");
    press(&mut app, KeyCode::PageDown);
    let paged = body_lines_on_screen(&app);
    let moved = paged
        .iter()
        .find(|(_, number)| *number == foot.1)
        .map(|(row, _)| *row)
        .unwrap_or_else(|| panic!("the foot of the first window is still on screen: {paged:?}"));
    assert_eq!(
        moved + 10,
        foot.0,
        "a page moves the view ten lines: {top:?} then {paged:?}"
    );

    press(&mut app, KeyCode::PageUp);
    assert_eq!(
        body_lines_on_screen(&app),
        top,
        "and pages back to where it was"
    );
}

/// Scrolling stops at the last line instead of running on past it, and one
/// more key at the end changes nothing.
#[test]
fn test_record_body_scroll_stops() {
    let mut app = record_app();
    open_record(&mut app);
    for _ in 0..40 {
        press(&mut app, KeyCode::Down);
    }
    let bottom = visible_body_lines(&app);
    assert_eq!(
        bottom.last(),
        Some(&40),
        "the body's last line is on screen: {bottom:?}"
    );
    press(&mut app, KeyCode::Down);
    assert_eq!(
        visible_body_lines(&app),
        bottom,
        "and it stays there: {bottom:?}"
    );
}

/// The record's own facts stay on screen while the body moves: the header says
/// which record is shown and the footer says how the keys act on it, however
/// far down the reader has scrolled.
#[test]
fn test_record_facts_stay_visible() {
    let mut app = record_app();
    open_record(&mut app);
    for _ in 0..40 {
        press(&mut app, KeyCode::Down);
    }
    assert!(shown(&app, "body-40"), "the body really did scroll");
    assert!(
        shown(&app, "Record 1 of 1"),
        "the header still names the record"
    );
    assert!(
        shown(&app, "Up/Down to scroll"),
        "and the footer still names the keys"
    );
}

/// The offset belongs to the record the body is of: the level opens at the top
/// of it every time, so a record is not shown part-way through.
#[test]
fn test_record_reopens_at_top() {
    let mut app = record_app();
    open_record(&mut app);
    for _ in 0..40 {
        press(&mut app, KeyCode::Down);
    }
    assert_ne!(visible_body_lines(&app).first(), Some(&1), "scrolled down");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        app.ui.trajectory.level(),
        2,
        "the drill is back on the record"
    );
    let reopened = visible_body_lines(&app);
    assert_eq!(
        reopened.first(),
        Some(&1),
        "the body opens at its first line: {reopened:?}"
    );
}

/// The record detail states which model call the record is, so the row can
/// keep its name column free of a competing ordinal.
#[test]
fn test_detail_names_model_call() {
    let mut first = record_of(TrajectoryRecordKind::Model, None);
    first.name = Some("deepseek-v4-pro-0813".into());
    first.ordinal = 1;
    let mut second = record_of(TrajectoryRecordKind::Model, None);
    second.name = Some("deepseek-v4.1-flash".into());
    second.ordinal = 2;
    let mut turn = turn(1, "ask");
    turn.models = vec![
        "deepseek-v4-pro-0813".to_string(),
        "deepseek-v4.1-flash".to_string(),
    ];
    let detail = detail_of_all(vec![first, second]);
    let (header, _, _, _) =
        detail::draw_event_detail(&turn, &detail, 1, Rect::new(0, 0, 140, 20), 0);
    let text: String = header
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("model call 2 of 2"),
        "the detail states the model-call ordinal: {text:?}"
    );
    assert!(
        text.contains("v4.1-flash"),
        "the header names the shortened model: {text:?}"
    );
}
