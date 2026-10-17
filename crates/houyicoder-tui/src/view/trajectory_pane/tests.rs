//! Rendering tests for the trajectory pane: the turn list, the drill levels,
//! and the time bars.

use super::list;

mod columns;
mod fixtures;
mod formatting;
mod scroll;
mod selection;
mod timeline;

use super::sample::sample_detail;
use super::*;
use crate::composition::app;
use crate::keys::handle_working;
use crate::state::Pane;
use crate::state::Screen;
use crate::test_harness::render_text;
use crate::view::working;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fixtures::{detail_of_all, record_of, turn, view};
use houyicoder_protocol::frontend::SlashCommand;
use ratatui::layout::Rect;
use ratatui::{Terminal, backend::TestBackend};
use std::env::temp_dir;
use std::fs::write;
use std::sync::{Arc, Mutex};

#[test]
fn test_sample_has_turns_events() {
    let t = sample_trajectory();
    assert!(t.total_turns > 0 && t.failures > 0);
    let key = match t.rows.first() {
        Some(TrajectoryRow::Turn(turn)) => turn.key.clone(),
        _ => panic!("a turn row"),
    };
    assert!(
        !sample_detail(&key).records.is_empty(),
        "the demonstration turn has records to drill into"
    );
}

#[test]
fn test_bar_width_proportional() {
    assert_eq!(bar_width(500, 1000, 40), 20);
    assert_eq!(bar_width(0, 1000, 40), 0);
    assert_eq!(bar_width(1000, 0, 40), 0);
}

#[test]
fn test_positioned_bar_fixed_width() {
    // Every bar must be exactly width chars or columns misalign.
    for w in 8..=60 {
        let s = positioned_bar(100, 500, 1000, w);
        assert_eq!(s.chars().count(), w, "width {w}");
    }
}

#[test]
fn test_positioned_bar_zero_total() {
    assert_eq!(positioned_bar(0, 100, 0, 20), " ".repeat(20));
}

#[test]
fn test_positioned_bar_instant_marker() {
    // A 0-duration event renders a thin marker at its offset, not a block.
    let s = positioned_bar(500, 0, 1000, 20);
    assert_eq!(s.chars().count(), 20);
    assert!(s.contains('┃'));
    assert!(!s.contains('█'));
}

#[test]
fn test_positioned_bar_spans_offset() {
    // A 200ms event at offset 100 on a 1000ms / 20-char axis: scale 0.02,
    // start col 2, end col 6 — 4 block chars, rest spaces.
    let s = positioned_bar(100, 200, 1000, 20);
    assert_eq!(s.chars().count(), 20);
    assert_eq!(s.chars().filter(|&c| c == '█').count(), 4);
    assert!(s.starts_with("  "));
}

#[test]
fn test_down_clamps_last_row() {
    // Adversarial: the bug was Down past the last row made the selection
    // glyph vanish (no row matched the out-of-range cursor). With clamping
    // in both render and the key handler, the cursor pins to the last row.
    let mut app = app();
    app.ui.navigation.pane = Pane::Trajectory;
    // First render stashes the list length; simulate by drawing once.
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            working::draw(f, &app);
        })
        .unwrap();
    let len = app.ui.trajectory.list_len();
    assert!(len > 0, "render must stash the list length");
    // Hammer Down past the end.
    for _ in 0..len + 5 {
        handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    assert_eq!(
        app.ui.trajectory.cursor(),
        len - 1,
        "cursor must clamp to last row, not exceed it"
    );
}

#[test]
fn test_pane_label_is_trajectory() {
    assert_eq!(Pane::Trajectory.label(), "trajectory");
}

#[test]
fn test_level0_renders_turn_list() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Trajectory;
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            working::draw(f, &app);
        })
        .unwrap();
}

/// A stub TrajectoryLog that flips a shared flag when called, so a test can
/// prove the render path read from the seam (not the mock fallback) when the
/// composition root attached an impl. The flag is shared via Arc so the test
/// reads it after the draw without downcasting the trait object.
struct StubLog {
    called: Arc<Mutex<bool>>,
}
impl TrajectoryLog for StubLog {
    fn trajectory(&self) -> Arc<TrajectoryView> {
        *self.called.lock().unwrap() = true;
        Arc::new(TrajectoryView {
            models_used: 2,
            session_id: "stub-session".into(),
            model: "stub-model".into(),
            timing: SessionTiming {
                ttft_samples: 1,
                ttft_avg_ms: None,
                ttft_p95_ms: None,
                ttft_p99_ms: None,
                decode_samples: 1,
                decode_tok_per_sec: None,
                model_ms: 0,
                tool_ms: 0,
            },
            ..view(Vec::new())
        })
    }

    fn load_older(&self) {}
    fn load_earliest(&self) {}
    fn return_to_tail(&self) {}
    fn request_detail(&self, _drill: &TrajectoryDrill) {}

    fn detail(&self, _key: &TrajectoryTurnKey) -> Arc<TrajectoryDetailView> {
        Arc::new(TrajectoryDetailView::default())
    }
}

#[test]
fn test_attached_seam_supplies_view() {
    // When the seam is Some, draw_content must call it (covering the Some
    // branch) rather than the mock fallback. The stub flips a shared flag on
    // call; a render pass leaves it set.
    let flag = Arc::new(Mutex::new(false));
    let mut app = app();
    app.ui.navigation.pane = Pane::Trajectory;
    app.sessions.persistence.trajectory_log = Some(Arc::new(StubLog {
        called: flag.clone(),
    }));
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            working::draw(f, &app);
        })
        .unwrap();
    assert!(
        *flag.lock().unwrap(),
        "draw_content must call the attached seam, not the mock fallback"
    );
}

#[test]
fn test_level1_renders_row_detail() {
    // Drilling a [bg] row (dream/compact/save) shows that row's detail at L1,
    // not the first turn's events. turn_idx 2 = the dream [bg] row in the mock.
    let mut app = app();
    app.ui.navigation.pane = Pane::Trajectory;
    app.ui.trajectory.set_level(1);
    app.ui.trajectory.set_turn_idx(2);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            working::draw(f, &app);
        })
        .unwrap();
    assert!(
        app.ui.trajectory.at_bg(),
        "L1 must flag the focused row as bg so Enter does not drill to L2"
    );
}

#[test]
fn test_level1_renders_turn_detail() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Trajectory;
    app.ui.trajectory.set_level(1);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            working::draw(f, &app);
        })
        .unwrap();
}

#[test]
fn test_level2_renders_event_detail() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Trajectory;
    app.ui.trajectory.set_level(2);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            working::draw(f, &app);
        })
        .unwrap();
}

/// L2 detail renders every field a record can carry, not just the ones a mock
/// happens to fill. Drives draw_event_detail over model and tool records
/// holding thinking, input, and output, and asserts each reaches the body. A
/// name-match table once masked an empty L2 body for real records while the
/// mock rendered fine, so these records carry the typed kinds the reader
/// emits rather than a name the draw path has to match on.
#[test]
fn test_level2_renders_record_detail() {
    fn ev(
        kind: TrajectoryRecordKind,
        thinking: Option<&str>,
        input: Option<&str>,
        output: Option<&str>,
    ) -> TrajectoryRecord {
        TrajectoryRecord {
            kind,
            name: Some("bash".into()),
            ordinal: 0,
            summary: "preview".into(),
            start_ms: 0,
            duration_ms: 10,
            outcome: RecordOutcome::Ok,
            thinking: thinking.map(Into::into),
            input: input.map(Into::into),
            output: output.map(Into::into),
            usage: None,
            timing: None,
            retries: 0,
        }
    }
    let turn = TrajectoryTurn {
        tokens_in: Some(0),
        tokens_out: Some(0),
        cache_read: Some(0),
        cache_write: Some(0),
        usage_provenance: UsageProvenance::ALL_REPORTED,
        tool_count: 2,
        ..turn(1, "real kinds")
    };
    let detail = detail_of_all(vec![
        ev(
            TrajectoryRecordKind::Model,
            Some("let me think"),
            None,
            None,
        ),
        ev(TrajectoryRecordKind::Tool, None, Some("echo hi"), None),
        ev(TrajectoryRecordKind::Tool, None, None, Some("hi")),
        ev(
            TrajectoryRecordKind::Model,
            Some("decided"),
            None,
            Some("the full reply"),
        ),
    ]);
    let body_text = |cursor: usize| {
        let (_, body, _, _) = detail::draw_event_detail(&turn, &detail, cursor, Rect::ZERO, 0);
        body.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
            .collect::<String>()
    };
    assert!(
        body_text(0).contains("let me think"),
        "a model record shows its thinking at L2"
    );
    assert!(
        body_text(1).contains("echo hi"),
        "a tool record shows its input at L2"
    );
    assert!(
        body_text(2).contains("hi"),
        "a tool record shows its output at L2"
    );
    let last = body_text(3);
    assert!(
        last.contains("decided"),
        "the last record shows its thinking"
    );
    assert!(
        last.contains("the full reply"),
        "and its full output, not just the preview"
    );
}

/// Gantt-bar visual invariants on the mock trajectory at the turn-detail
/// level: unicode block bars render (█), the selection glyph pins the
/// focused row (▸), and the mock's content (a "cargo test" call) shows.
/// The sample is the only data with non-zero duration_ms (hardcoded in
/// the sample fixture); the real binary always wires a real SessionLog
/// whose fresh session has zero turns, so the bars are unreachable on the
/// real-binary PTY path — this unit test holds the bar invariants where
/// they are reachable, and dumps the rendered level to a temp file for
/// human visual review (the Gantt timeline, the cursor row, the unicode
/// bars vs ASCII hashes).
#[test]
fn test_trajectory_bar_invariants_mock() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Trajectory;
    app.ui.trajectory.set_level(1);
    let out = render_text(&app, 100, 40);
    assert!(
        out.contains('█'),
        "unicode block bar must render at the turn-detail level:\n{out}"
    );
    assert!(
        out.contains('▸'),
        "selection glyph must pin the focused row:\n{out}"
    );
    assert!(
        out.contains("cargo test"),
        "the mock's tool-call content must render:\n{out}"
    );
    let path = temp_dir().join("houyi-trajectory-capture.txt");
    write(&path, &out).unwrap();
}

/// Secrets in tool I/O must NOT render on the trajectory pane (a human-facing
/// surface — screen-share / recording / scrollback). The L2 event detail
/// redacts input/output/thinking before drawing; the durable log the pane
/// reads stays full-fidelity. This pins the redact-on-read boundary.
#[test]
fn test_event_detail_redacts_secrets() {
    let secret = "sk-abcd1234efgh5678ijkl9012mnop3456qrst";
    let turn = TrajectoryTurn {
        tokens_in: Some(0),
        tokens_out: Some(0),
        cache_read: Some(0),
        cache_write: Some(0),
        usage_provenance: UsageProvenance::ALL_REPORTED,
        tool_count: 1,
        ..turn(1, "show keys")
    };
    let detail = detail_of_all(vec![TrajectoryRecord {
        kind: TrajectoryRecordKind::Tool,
        name: Some("read".into()),
        ordinal: 0,
        summary: "creds".into(),
        start_ms: 0,
        duration_ms: 10,
        outcome: RecordOutcome::Ok,
        thinking: None,
        input: None,
        output: Some(format!("token={secret}")),
        usage: None,
        timing: None,
        retries: 0,
    }]);
    let (_, body, _, _) = detail::draw_event_detail(&turn, &detail, 0, Rect::ZERO, 0);
    let text = body
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
        .collect::<String>();
    assert!(
        text.contains("[REDACTED"),
        "the secret must be redacted in the pane, got: {text}"
    );
    assert!(
        !text.contains(secret),
        "the raw secret must not render in the pane, got: {text}"
    );
}

#[test]
fn test_enter_drill_esc_back() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Trajectory;
    // Render once so the turn-list length is stashed — the drill guard reads
    // it to decide whether Enter may drill (it must not drill into an empty
    // row list). Without this render the stashed length is 0 and the guard
    // holds the pane at the turn-list level.
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| working::draw(f, &app)).unwrap();
    assert_eq!(app.ui.trajectory.level(), 0);
    handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.ui.trajectory.level(), 1);
    handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.ui.trajectory.level(), 2);
    handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.ui.trajectory.level(), 1);
    handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.ui.trajectory.level(), 0);
    handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.ui.navigation.pane, Pane::Transcript);
}

#[test]
fn test_up_down_move_cursor() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Trajectory;
    app.ui.trajectory.set_list_len(5);
    assert_eq!(app.ui.trajectory.cursor(), 0);
    // Down advances
    handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.ui.trajectory.cursor(), 1);
    // Up decrements
    handle_working(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.ui.trajectory.cursor(), 0);
    // Up at the top stays at the top: the list is an audit trail with a
    // direction in time, so it does not wrap.
    handle_working(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.ui.trajectory.cursor(), 0);
    // With no paged history attached, End and Home are the ends of the list in
    // hand, and Down at the end stays put.
    handle_working(&mut app, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(app.ui.trajectory.cursor(), 4);
    handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.ui.trajectory.cursor(), 4);
    // Home jumps to the oldest.
    handle_working(&mut app, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(app.ui.trajectory.cursor(), 0);
}

/// The thinking cell states the reasoning span and the tokens together, so a
/// turn that thought for seconds is visible in the list without a drill.
#[test]
fn test_thinking_cell_states_both() {
    let view = TrajectoryView {
        models_used: 2,
        tokens_in: Some(100),
        tokens_out: Some(50),
        ..view(vec![TrajectoryRow::Turn(TrajectoryTurn {
            tokens_in: Some(100),
            tokens_out: Some(50),
            cache_read: Some(0),
            cache_write: Some(0),
            usage_provenance: UsageProvenance::ALL_REPORTED,
            reasoning_tokens: Some(541),
            reasoning_ms: Some(4_200),
            ..turn(1, "hi")
        })])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("(thinking 4.2s · 541)"),
        "the span and the tokens are stated together: {text}"
    );
}

/// A call that reported a reasoning span but no reasoning token count states
/// the span alone rather than inventing a count beside it.
#[test]
fn test_thinking_cell_states_span() {
    let view = TrajectoryView {
        models_used: 2,
        tokens_in: Some(100),
        tokens_out: Some(50),
        ..view(vec![TrajectoryRow::Turn(TrajectoryTurn {
            tokens_in: Some(100),
            tokens_out: Some(50),
            cache_read: Some(0),
            cache_write: Some(0),
            usage_provenance: UsageProvenance::ALL_REPORTED,
            reasoning_ms: Some(4_200),
            ..turn(1, "hi")
        })])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 20));
    let cell = body[0]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .find(|c| c.contains("thinking"))
        .expect("the row states the reasoning span");
    assert_eq!(
        cell.trim(),
        "(thinking 4.2s)",
        "the span is stated alone, with no count invented beside it"
    );
}

/// Thinking tokens render as (thinking Nk) only when Some and >0.
#[test]
fn test_thinking_cell_states_tokens() {
    let view = TrajectoryView {
        models_used: 2,
        tokens_in: Some(100),
        tokens_out: Some(50),
        ..view(vec![TrajectoryRow::Turn(TrajectoryTurn {
            tokens_in: Some(100),
            tokens_out: Some(50),
            cache_read: Some(0),
            cache_write: Some(0),
            usage_provenance: UsageProvenance::ALL_REPORTED,
            reasoning_tokens: Some(20),
            ..turn(1, "hi")
        })])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("(thinking 20)"),
        "tokens shown when >0: {text}"
    );
}

/// A turn that reported neither figure states no thinking cell, while a turn
/// beside it that did report one draws the column: the absence belongs to the
/// cell, not to a column the layout dropped for want of width.
#[test]
fn test_thinking_cell_hidden_unmeasured() {
    let mk = |n: usize, ms: Option<u64>| TrajectoryTurn {
        tokens_in: Some(100),
        tokens_out: Some(50),
        duration_ms: 100,
        reasoning_ms: ms,
        ..turn(n, "hi")
    };
    let view = TrajectoryView {
        models_used: 2,
        tokens_in: Some(200),
        tokens_out: Some(100),
        ..view(vec![
            TrajectoryRow::Turn(mk(1, None)),
            TrajectoryRow::Turn(mk(2, Some(1_200))),
        ])
    };
    // Past the width at which the thinking column joins, so a blank cell on the
    // first row is the cell's own doing. Below that width the column is dropped
    // whole and the assertion would hold whatever the cell returned.
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 20));
    let row_text = |i: usize| -> String {
        body[i]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    };
    assert!(
        row_text(1).contains("thinking"),
        "the column holds at this width, so the assertion below is about the cell: {}",
        row_text(1)
    );
    assert!(
        !row_text(0).contains("thinking"),
        "a turn that reported neither figure states no thinking cell: {}",
        row_text(0)
    );
}

/// A span the call reported is stated whatever it rounds to: a block that
/// closed inside a millisecond is a measurement, not the absence a missing span
/// is, and stating it does not displace a count reported beside it.
#[test]
fn test_thinking_zero_stated() {
    let thinking_cell = |tokens: Option<usize>| -> String {
        let view = TrajectoryView {
            models_used: 2,
            tokens_in: Some(100),
            tokens_out: Some(50),
            ..view(vec![TrajectoryRow::Turn(TrajectoryTurn {
                tokens_in: Some(100),
                tokens_out: Some(50),
                cache_read: Some(0),
                cache_write: Some(0),
                usage_provenance: UsageProvenance::ALL_REPORTED,
                reasoning_ms: Some(0),
                reasoning_tokens: tokens,
                ..turn(1, "hi")
            })])
        };
        let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 20));
        body[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .find(|c| c.contains("thinking"))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    assert_eq!(
        thinking_cell(None),
        "(thinking 0ms)",
        "a span measured below a millisecond is stated as the measurement it is"
    );
    assert_eq!(
        thinking_cell(Some(20)),
        "(thinking 0ms · 20)",
        "the count is stated beside the span it accompanied"
    );
}

/// Per-turn model renders only when ≥2 distinct models in the session.
#[test]
fn test_per_turn_model_two() {
    let view = TrajectoryView {
        models_used: 2,
        model: "2 models".into(),
        tokens_in: Some(200),
        tokens_out: Some(100),
        ..view(vec![
            TrajectoryRow::Turn(TrajectoryTurn {
                tokens_in: Some(100),
                tokens_out: Some(50),
                cache_read: Some(0),
                cache_write: Some(0),
                usage_provenance: UsageProvenance::ALL_REPORTED,
                models: vec!["qwen3.7-max".into()],
                efforts: vec!["high".into()],
                ..turn(1, "a")
            }),
            TrajectoryRow::Turn(TrajectoryTurn {
                tokens_in: Some(100),
                tokens_out: Some(50),
                cache_read: Some(0),
                cache_write: Some(0),
                usage_provenance: UsageProvenance::ALL_REPORTED,
                models: vec!["glm-5.2".into()],
                ..turn(2, "b")
            }),
        ])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("qwen3.7-max"), "model id on turn 1: {text}");
    assert!(text.contains("glm-5.2"), "model id on turn 2: {text}");
    assert!(text.contains("high"), "effort on turn 1: {text}");
}

/// Per-turn model is hidden when the session used only one model.
#[test]
fn test_per_turn_model_one() {
    let view = TrajectoryView {
        models_used: 2,
        model: "qwen3.7-max".into(),
        tokens_in: Some(200),
        tokens_out: Some(100),
        ..view(vec![
            TrajectoryRow::Turn(TrajectoryTurn {
                tokens_in: Some(100),
                tokens_out: Some(50),
                cache_read: Some(0),
                cache_write: Some(0),
                usage_provenance: UsageProvenance::ALL_REPORTED,
                models: vec!["qwen3.7-max".into()],
                ..turn(1, "a")
            }),
            TrajectoryRow::Turn(TrajectoryTurn {
                tokens_in: Some(100),
                tokens_out: Some(50),
                cache_read: Some(0),
                cache_write: Some(0),
                usage_provenance: UsageProvenance::ALL_REPORTED,
                models: vec!["qwen3.7-max".into()],
                ..turn(2, "b")
            }),
        ])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 100, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        !text.contains("qwen3.7-max"),
        "per-turn model hidden when single: {text}"
    );
}

/// Turn rows render the per-turn cached ratio.
#[test]
fn test_turn_row_cached_ratio() {
    let view = TrajectoryView {
        models_used: 2,
        tokens_in: Some(200),
        tokens_out: Some(100),
        ..view(vec![
            TrajectoryRow::Turn(TrajectoryTurn {
                tokens_in: Some(100),
                tokens_out: Some(50),
                cache_read: Some(0),
                cache_write: Some(0),
                usage_provenance: UsageProvenance::ALL_REPORTED,
                ..turn(2, "first")
            }),
            TrajectoryRow::Turn(TrajectoryTurn {
                tokens_in: Some(100),
                tokens_out: Some(50),
                cache_read: Some(50),
                cache_write: Some(0),
                usage_provenance: UsageProvenance::ALL_REPORTED,
                tool_count: 1,
                duration_ms: 500,
                ..turn(1, "second")
            }),
        ])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 100, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("50% cache"),
        "the per-turn cached ratio renders: {text}"
    );
    assert!(
        !text.contains('─'),
        "turn numbering is monotonic, so no boundary separator is drawn: {text}"
    );
}

/// A cache-read count of zero that no call reported is unknown, not a measured
/// miss, so the row claims no share for it.
#[test]
fn test_row_hides_unreported_zero() {
    let view = TrajectoryView {
        models_used: 1,
        tokens_in: Some(100),
        tokens_out: Some(50),
        ..view(vec![TrajectoryRow::Turn(TrajectoryTurn {
            tokens_in: Some(100),
            tokens_out: Some(50),
            cache_read: Some(0),
            cache_write: Some(0),
            usage_provenance: UsageProvenance::default(),
            ..turn(1, "legacy")
        })])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 100, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        !text.contains("cached"),
        "an unreported zero claims no share: {text}"
    );
}

/// A turn whose log left the cache-read count at zero and whose call never
/// reported it states no cache line at all: the header says nothing rather than
/// naming a share over a count that was never measured.
#[test]
fn test_header_hides_unreported_zero() {
    let legacy = TrajectoryTurn {
        tokens_in: Some(100),
        tokens_out: Some(50),
        cache_read: Some(0),
        cache_write: Some(0),
        usage_provenance: UsageProvenance::default(),
        ..turn(1, "legacy")
    };
    let detail = detail_of_all(Vec::new());
    let app = app();
    let (header, _, _, _) =
        detail::draw_turn_detail(&legacy, &detail, 0, Rect::new(0, 0, 140, 20), &app);
    let head: String = header
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        !head.contains("cache hit"),
        "the header claims nothing for an unreported zero: {head}"
    );
    assert!(
        !head.contains('%'),
        "no share is stated where nothing was measured: {head}"
    );
    assert!(
        head.contains("100 in 50 out"),
        "the turn's own counts still render: {head}"
    );
}

/// A cache-read count above zero whose account is incomplete keeps its count and
/// loses its share: the quotient over a partial account can read higher than the
/// truth, so the row claims none while the header still names the count.
#[test]
fn test_row_hides_unreported_share() {
    let partial = TrajectoryTurn {
        tokens_in: Some(1_000),
        tokens_out: Some(50),
        cache_read: Some(400),
        cache_write: Some(0),
        usage_provenance: UsageProvenance::default(),
        ..turn(1, "inherited")
    };
    let view = TrajectoryView {
        models_used: 1,
        tokens_in: Some(1_000),
        tokens_out: Some(50),
        ..view(vec![TrajectoryRow::Turn(partial.clone())])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 100, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        !text.contains("cached"),
        "a partial account claims no share: {text}"
    );

    let detail = detail_of_all(Vec::new());
    let app = app();
    let (header, _, _, _) =
        detail::draw_turn_detail(&partial, &detail, 0, Rect::new(0, 0, 140, 20), &app);
    let head: String = header
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        head.contains("400 cache hit"),
        "the count itself still reaches the header: {head}"
    );
    assert!(
        !head.contains('%'),
        "no percentage is stated over a partial account: {head}"
    );
}

/// A miss the provider measured is worth seeing: the row states the zero, and
/// the drill's header derives the same share from the same turn, so the two
/// renderers cannot disagree about it.
#[test]
fn test_measured_miss_is_stated() {
    let measured = TrajectoryTurn {
        tokens_in: Some(1_000),
        tokens_out: Some(50),
        cache_read: Some(0),
        cache_write: Some(0),
        usage_provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        },
        ..turn(1, "fresh prefix")
    };
    let view = TrajectoryView {
        models_used: 1,
        tokens_in: Some(1_000),
        tokens_out: Some(50),
        ..view(vec![TrajectoryRow::Turn(measured.clone())])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 100, 20));
    let row: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(row.contains("0% cached"), "the row states the miss: {row}");

    let detail = detail_of_all(Vec::new());
    let app = app();
    let (header, _, _, _) =
        detail::draw_turn_detail(&measured, &detail, 0, Rect::new(0, 0, 140, 20), &app);
    let head: String = header
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        head.contains("0% cache hit"),
        "the header states it too: {head}"
    );
    assert!(!head.contains("(0%)"), "a miss names no count: {head}");
}

/// The session header derives its share from the account the folds own, not
/// from the two counts alone: an account the endpoints did not fully report
/// leaves the cache-read total a floor, and dividing it by an input total short
/// by whatever went unreported would read as a share the session never had.
#[test]
fn test_session_header_hides_share() {
    let view = TrajectoryView {
        tokens_in: Some(1_000),
        tokens_out: Some(200),
        cache_read: Some(500),
        usage_provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: false,
            cache_write_reported: false,
        },
        ..view(Vec::new())
    };
    let (header, _, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 100, 25));
    let head: String = header
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        !head.contains("cache hit"),
        "a partial account states no share: {head}"
    );
    assert!(!head.contains('%'), "and no percentage: {head}");
    assert!(
        head.contains("1k in 200 out"),
        "the counts themselves still render: {head}"
    );
}

/// A session whose endpoints reported the cache-read count and reused nothing
/// states the miss: a reported zero is a measurement, not a missing value.
#[test]
fn test_session_header_states_miss() {
    let view = TrajectoryView {
        tokens_in: Some(1_000),
        tokens_out: Some(200),
        cache_read: Some(0),
        usage_provenance: UsageProvenance::ALL_REPORTED,
        ..view(Vec::new())
    };
    let (header, _, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 100, 25));
    let head: String = header
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(head.contains("cache hit 0%"), "the miss is stated: {head}");
}

/// Entering trajectory initializes cursor to tail, and draw clamps and persists it.
#[test]
fn test_enter_trajectory_clamps_tail() {
    let mut app = app();
    app.run_command(SlashCommand::Trajectory);
    assert_eq!(app.ui.trajectory.cursor(), usize::MAX);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|f| {
            draw_content(f, f.area(), &app);
        })
        .unwrap();
    // After first draw, cursor is clamped to valid row index
    let len = app.ui.trajectory.list_len();
    assert!(app.ui.trajectory.cursor() < len);
    // Enter drills into Level 1 without displaying "no row data"
    handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.ui.trajectory.level(), 1);
}

/// Level 1 navigation properly uses events length, allowing Down to advance.
#[test]
fn test_level1_navigates_events() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Trajectory;
    app.ui.trajectory.set_level(1);
    app.ui.trajectory.set_turn_idx(0);
    app.ui.trajectory.set_cursor(0);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|f| {
            draw_content(f, f.area(), &app);
        })
        .unwrap();
    let event_count = app.ui.trajectory.list_len();
    assert!(event_count > 1, "mock turn 0 has multiple events");
    // Down advances through events
    handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.ui.trajectory.cursor(), 1);
}

/// Latency timing spans and cache metrics in trajectory view render cleanly.
#[test]
fn test_timing_and_cache_render() {
    let view = TrajectoryView {
        models_used: 2,
        session_id: "s1".into(),
        tokens_in: Some(1000),
        tokens_out: Some(200),
        cache_read: Some(500),
        duration_ms: Some(10_000),
        timing: SessionTiming {
            ttft_samples: 1,
            ttft_avg_ms: Some(250),
            ttft_p95_ms: Some(400),
            ttft_p99_ms: Some(600),
            decode_samples: 1,
            decode_tok_per_sec: Some(45.2),
            model_ms: 0,
            tool_ms: 0,
        },
        ..view(vec![TrajectoryRow::Turn(TrajectoryTurn {
            tokens_in: Some(1000),
            tokens_out: Some(200),
            cache_read: Some(500),
            duration_ms: 1000,
            ..turn(1, "test")
        })])
    };
    let (header, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 100, 25));
    let head_text: String = header
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        head_text.contains("TTFT avg 250ms"),
        "a sub-second sample reads in milliseconds: {head_text}"
    );
    assert!(head_text.contains("p95 400ms"), "{head_text}");
    assert!(head_text.contains("p99 600ms"), "{head_text}");
    assert!(head_text.contains("decode 45.2 tok/s"));
    assert!(head_text.contains("cache hit 50%"));

    let body_text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(body_text.contains("50% cached"));
}

/// Every outcome has its own glyph and colour, so a pending record never looks
/// finished and a failure never looks clean.
#[test]
fn test_outcome_glyphs() {
    assert_eq!(RecordOutcome::Ok.glyph(), "✓");
    assert_eq!(RecordOutcome::Failed.glyph(), "✗");
    assert_eq!(RecordOutcome::Pending.glyph(), "…");
}

/// Every record kind has a stable label for the L1 kind column.
#[test]
fn test_record_kind_labels() {
    let labels: Vec<&str> = [
        TrajectoryRecordKind::Context,
        TrajectoryRecordKind::Model,
        TrajectoryRecordKind::Tool,
        TrajectoryRecordKind::Agent,
        TrajectoryRecordKind::Memory,
        TrajectoryRecordKind::Hook,
        TrajectoryRecordKind::Compaction,
        TrajectoryRecordKind::Error,
    ]
    .iter()
    .map(|k| k.label())
    .collect();
    assert_eq!(
        labels,
        vec![
            "context", "model", "tool", "agent", "memory", "hook", "compact", "error"
        ]
    );
}

/// A model record's detail shows the latency split and the provider usage it
/// carries, and omits a part the log did not record rather than printing zero.
#[test]
fn test_detail_shows_model_facts() {
    let mut record = record_of(TrajectoryRecordKind::Model, Some("answer"));
    record.name = Some("qwen3.7-max".into());
    record.timing = Some(EventTiming {
        total_ms: 620,
        ttft_ms: Some(210),
        decode_ms: Some(410),
        reasoning_ms: None,
        response_ms: None,
    });
    record.usage = Some(EventUsage {
        input: Some(1200),
        output: Some(340),
        cache_read: Some(1000),
        cache_write: None,
        reasoning: Some(90),
        provenance: UsageProvenance::ALL_REPORTED,
    });
    record.retries = 1;
    let turn = TrajectoryTurn {
        tokens_in: Some(1200),
        tokens_out: Some(340),
        cache_read: Some(1000),
        models: vec!["qwen3.7-max".into()],
        reasoning_tokens: Some(90),
        retries: 1,
        duration_ms: 620,
        ..turn(1, "ask")
    };
    let detail = detail_of_all(vec![record]);
    let (header, body, _, _) = detail::draw_event_detail(&turn, &detail, 0, Rect::ZERO, 0);
    let text: String = header
        .iter()
        .chain(body.iter())
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("qwen3.7-max"),
        "the model id is named: {text}"
    );
    assert!(text.contains("TTFT 210ms"), "the split is shown: {text}");
    assert!(text.contains("decode 410ms"), "and its decode part: {text}");
    assert!(text.contains("cache read 1000"), "usage is shown: {text}");
    assert!(
        !text.contains("cache write"),
        "an unrecorded part stays absent: {text}"
    );
    assert!(
        text.contains("1 length recovery"),
        "retries are shown: {text}"
    );
}

/// Per-call counts obey the same rule as the turn row: a zero the call's
/// endpoint reported is a measured miss and stated, one it never reported is
/// unknown and stays off the drill, and a count whose event carries no report
/// flag is stated only above zero.
#[test]
fn test_call_cache_follows_report() {
    let call = |usage: EventUsage| -> String {
        let mut record = record_of(TrajectoryRecordKind::Model, Some("answer"));
        record.name = Some("qwen3.7-max".into());
        record.usage = Some(usage);
        let turn = turn(1, "ask");
        let detail = detail_of_all(vec![record]);
        let (header, body, _, _) = detail::draw_event_detail(&turn, &detail, 0, Rect::ZERO, 0);
        header
            .iter()
            .chain(body.iter())
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect()
    };
    let reported = UsageProvenance {
        input_reported: true,
        cache_read_reported: true,
        cache_write_reported: false,
    };
    let measured = call(EventUsage {
        input: Some(1_200),
        output: Some(340),
        cache_read: Some(0),
        cache_write: None,
        reasoning: None,
        provenance: reported,
    });
    assert!(
        measured.contains("cache read 0"),
        "the miss is stated: {measured}"
    );
    let unnamed = call(EventUsage {
        input: Some(1_200),
        output: Some(340),
        cache_read: Some(0),
        cache_write: None,
        reasoning: None,
        provenance: UsageProvenance::default(),
    });
    assert!(
        !unnamed.contains("cache read"),
        "an unreported zero stays off the drill: {unnamed}"
    );

    // A call whose endpoint reported nothing carries no count to print: the
    // output and reasoning totals have no report flag on the record, so a zero
    // reads as unknown there just as an unreported breakdown count does.
    let silent = call(EventUsage {
        input: Some(0),
        output: Some(0),
        cache_read: Some(0),
        cache_write: Some(0),
        reasoning: Some(0),
        provenance: UsageProvenance::default(),
    });
    assert!(
        !silent.contains("tokens:"),
        "no count column is drawn from zeroes nobody reported: {silent}"
    );
    assert!(
        !silent.contains("reasoning:"),
        "an unreported reasoning zero stays off too: {silent}"
    );
}

/// The L1 timeline names what each record acted on, so a tool row reads as the
/// tool it ran rather than as a generic row.
#[test]
fn test_timeline_shows_record_names() {
    let turn = TrajectoryTurn {
        tokens_in: Some(0),
        tokens_out: Some(0),
        tool_count: 1,
        duration_ms: 100,
        ..turn(1, "go")
    };
    let detail = detail_of_all(vec![record_of(TrajectoryRecordKind::Tool, None)]);
    let app = app();
    let (_, body, _, _) =
        detail::draw_turn_detail(&turn, &detail, 0, Rect::new(0, 0, 120, 20), &app);
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("tool"), "the kind column is labelled: {text}");
    assert!(
        text.contains("bash"),
        "the tool name is shown next to it: {text}"
    );
}

/// A clear between two turns draws a separator above the turn it precedes, with
/// when it happened, and the separator is not a selectable row.
#[test]
fn test_turn_list_boundary() {
    let turn = TrajectoryTurn {
        boundary_before: vec![TurnBoundary::ContextCleared {
            prior_turn: 1,
            at_secs: now_epoch_secs().saturating_sub(120),
        }],
        tokens_in: Some(10),
        tokens_out: Some(5),
        cache_read: Some(0),
        duration_ms: 100,
        ..turn(2, "after clear")
    };
    let view = TrajectoryView {
        models_used: 2,
        tokens_in: Some(10),
        tokens_out: Some(5),
        ..view(vec![TrajectoryRow::Turn(turn)])
    };
    let (_, body, _, sel_line) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 120, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("context cleared"),
        "the boundary is drawn: {text}"
    );
    assert!(text.contains("2m ago"), "with when it happened: {text}");
    assert_eq!(
        sel_line,
        BodyScroll::Cursor(1),
        "the selected row sits below the separator, so the cursor's body line \
         is not the row index"
    );
}

/// The turn list pads its columns, so the numbers line up down the list.
#[test]
fn test_turn_list_column_align() {
    let mk = |n: usize, title: &str, tin: Option<usize>| TrajectoryTurn {
        tokens_in: tin,
        tokens_out: Some(5),
        cache_read: Some(0),
        duration_ms: 100,
        ..turn(n, title)
    };
    let view = TrajectoryView {
        models_used: 2,
        tokens_in: Some(10),
        tokens_out: Some(10),
        ..view(vec![
            TrajectoryRow::Turn(mk(1, "short", Some(1))),
            TrajectoryRow::Turn(mk(2, "a much longer title that is cut", Some(1_200))),
        ])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 120, 20));
    let lines: Vec<String> = body
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    // The padded prefix is the same width on both rows, so a long title cannot
    // push the numbers out of line.
    let head = |s: &str| s.chars().take(56).collect::<String>();
    assert_eq!(
        head(&lines[0]).chars().count(),
        head(&lines[1]).chars().count()
    );
}

/// A model row at Level 1 states its measured split: the first-token wait and
/// the decode rate of the tokens it produced.
#[test]
fn test_turn_detail_latency_split() {
    let mut record = record_of(TrajectoryRecordKind::Model, Some("answer"));
    record.name = Some("qwen3.7-max".into());
    record.ordinal = 1;
    record.timing = Some(EventTiming {
        total_ms: 620,
        ttft_ms: Some(210),
        decode_ms: Some(410),
        reasoning_ms: None,
        response_ms: None,
    });
    record.usage = Some(EventUsage {
        input: Some(10),
        output: Some(400),
        cache_read: None,
        cache_write: None,
        reasoning: None,
        provenance: UsageProvenance::ALL_REPORTED,
    });
    let turn = TrajectoryTurn {
        tokens_in: Some(10),
        tokens_out: Some(400),
        models: vec!["qwen3.7-max".into()],
        duration_ms: 620,
        ..turn(1, "ask")
    };
    let detail = detail_of_all(vec![record]);
    let app = app();
    let (_, body, _, _) =
        detail::draw_turn_detail(&turn, &detail, 0, Rect::new(0, 0, 140, 20), &app);
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("TTFT 210ms"), "the wait is shown: {text}");
    assert!(text.contains("975.6 tok/s"), "and the rate: {text}");
    assert!(
        text.contains("1 qwen3.7"),
        "the call is numbered in the name column: {text}"
    );
}

/// The turn list drops its least informative columns as the terminal narrows,
/// so the duration and the outcome always survive; a padded column counts
/// display columns, so a wide glyph cannot shift the columns after it.
#[test]
fn test_turn_list_degrades() {
    let turn = |n: usize, title: &str, model: &str| TrajectoryTurn {
        n,
        key: TrajectoryTurnKey::from_opening_event(&format!("t{n}")),
        boundary_before: Vec::new(),
        title: title.into(),
        tokens_in: Some(1_200),
        tokens_out: Some(500),
        cache_read: Some(1_000),
        cache_write: None,
        usage_provenance: UsageProvenance::ALL_REPORTED,
        models: vec![model.into()],
        efforts: vec!["high".into()],
        reasoning_tokens: Some(90),
        reasoning_ms: Some(4_200),
        tool_count: 3,
        tool_fail: 1,
        retries: 0,
        duration_ms: 12_400,
        success: false,
    };
    // Two distinct models, so the per-turn model column is drawn at all.
    let view = TrajectoryView {
        models_used: 2,
        model: "2 models".into(),
        tokens_in: Some(2_400),
        tokens_out: Some(1_000),
        failures: 2,
        duration_ms: Some(24_000),
        cache_read: Some(2_000),
        ..view(vec![
            TrajectoryRow::Turn(turn(12, "宽的标题会让列错位", "qwen3.7-max")),
            TrajectoryRow::Turn(turn(13, "ascii title", "glm-5.2")),
        ])
    };
    let text_at = |w: u16| -> String {
        let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, w, 10));
        body.iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect()
    };
    let wide = text_at(200);
    assert!(wide.contains("12.4s"), "duration present when wide: {wide}");
    assert!(wide.contains('✗'), "outcome present when wide: {wide}");
    assert!(wide.contains("3 calls"), "call count when wide: {wide}");
    assert!(wide.contains("qwen3.7-max"), "model when wide: {wide}");
    // Narrow: the low-priority columns are gone, the duration is not.
    let narrow = text_at(90);
    assert!(
        narrow.contains("12.4s"),
        "duration survives narrow: {narrow}"
    );
    assert!(narrow.contains('✗'), "outcome survives narrow: {narrow}");
    assert!(
        !narrow.contains("3 calls"),
        "the call column is the first to go: {narrow}"
    );
    assert!(
        !narrow.contains("qwen3.7-max"),
        "so is the model column: {narrow}"
    );
}

/// A model switch and a compaction each draw their own separator above the turn
/// they precede, so a latency or cache shift across models is explainable.
#[test]
fn test_turn_list_other_boundaries() {
    let mk = |n: usize, boundary: TurnBoundary| TrajectoryTurn {
        boundary_before: vec![boundary],
        tokens_in: Some(10),
        tokens_out: Some(5),
        cache_read: Some(0),
        duration_ms: 100,
        ..turn(n, &format!("turn {n}"))
    };
    let view = TrajectoryView {
        models_used: 2,
        model: "2 models".into(),
        tokens_in: Some(20),
        tokens_out: Some(10),
        ..view(vec![
            TrajectoryRow::Turn(mk(
                1,
                TurnBoundary::ModelSwitch(Box::new(ModelSwitchBoundary {
                    from: "qwen".into(),
                    to: "deepseek".into(),
                    at_secs: 0,
                })),
            )),
            TrajectoryRow::Turn(mk(
                2,
                TurnBoundary::Compacted(Box::new(CompactedBoundary {
                    checkpoint_id: "ck-1".into(),
                    pre_tokens: 128_000,
                    post_tokens: 32_000,
                    at_secs: 0,
                })),
            )),
        ])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("model qwen → deepseek"),
        "the model switch is named: {text}"
    );
    assert!(
        text.contains("context compacted 128k → 32k (checkpoint ck-1)"),
        "the compaction names its checkpoint and what it reclaimed: {text}"
    );
}

#[test]
fn test_compaction_without_counts() {
    // A log written before the boundary carried token counts deserializes to
    // zeroes. The row then names the checkpoint without a bracket, rather than
    // printing a fold from nothing to nothing.
    let mk = |n: usize, boundary: TurnBoundary| TrajectoryTurn {
        boundary_before: vec![boundary],
        tokens_in: Some(10),
        tokens_out: Some(5),
        cache_read: Some(0),
        duration_ms: 100,
        ..turn(n, &format!("turn {n}"))
    };
    let view = TrajectoryView {
        models_used: 2,
        tokens_in: Some(10),
        tokens_out: Some(5),
        ..view(vec![TrajectoryRow::Turn(mk(
            1,
            TurnBoundary::Compacted(Box::new(CompactedBoundary {
                checkpoint_id: "ck-1".into(),
                pre_tokens: 0,
                post_tokens: 0,
                at_secs: 0,
            })),
        ))])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("context compacted (checkpoint ck-1)"),
        "an old boundary keeps its checkpoint label: {text}"
    );
    assert!(
        !text.contains("→"),
        "and claims no fold it cannot show: {text}"
    );
}

/// A page that has not landed is its own state: the list must say so rather
/// than render an empty table, which would read as a session with no turns.
#[test]
fn test_list_renders_read_states() {
    let area = Rect::new(0, 0, 100, 30);
    for (state, needle) in [
        (TrajectoryViewState::Loading, "loading trajectory"),
        (TrajectoryViewState::LoadingOlder, "loading older turns"),
        (
            TrajectoryViewState::Failed,
            "could not read trajectory history",
        ),
    ] {
        let mut view = sample_trajectory();
        view.state = state;
        view.rows.clear();
        let (_header, body, _footer, _sel) = list::draw_turn_list(&view, 0, area);
        let text: String = body.iter().map(|line| line.to_string()).collect();
        assert!(
            text.contains(needle),
            "{state:?} renders its own line: {text:?}"
        );
        // Only the states with nothing to list replace the body; a read of
        // older turns adds a line above the rows already loaded.
        if state != TrajectoryViewState::LoadingOlder {
            assert_eq!(body.len(), 1, "{state:?} shows only its line");
        }
    }
}

/// A read of older turns must not hide the rows already loaded: the window
/// keeps what it has and the pane adds a line saying more is on its way.
#[test]
fn test_loading_older_keeps_rows() {
    let mut view = sample_trajectory();
    view.state = TrajectoryViewState::LoadingOlder;
    let rows_before = view.rows.len();
    let area = Rect::new(0, 0, 120, 40);
    let (_header, body, _footer, _sel) = list::draw_turn_list(&view, 0, area);
    let text: String = body.iter().map(|line| line.to_string()).collect();
    assert!(text.contains("loading older turns"), "{text:?}");
    assert!(
        body.len() > rows_before,
        "the loaded rows are still rendered: {} lines for {rows_before} rows",
        body.len()
    );
}

/// A turn's key is the durable id it was built from, unchanged: the pane only
/// compares keys, and the composition root resolves one back to the bytes.
#[test]
fn test_turn_key_round_trips() {
    let key = TrajectoryTurnKey::from_opening_event("01J0-opened-the-turn");
    assert_eq!(key.as_str(), "01J0-opened-the-turn");
    assert_eq!(key, key.clone(), "a key compares by the id it holds");
}
