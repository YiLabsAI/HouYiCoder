//! Tests for the trajectory pane, extracted to keep the render module under
//! the file-size gate. Included from trajectory_pane.rs via a path attribute so
//! the tests still see the parent module's private items via super::*.

use super::*;
use ratatui::{Terminal, backend::TestBackend};

#[test]
fn test_mock_has_turns_events() {
    let t = mock_trajectory();
    assert!(t.total_turns > 0 && t.failures > 0);
    assert!(
        t.rows
            .iter()
            .any(|r| matches!(r, TrajectoryRow::Turn(t) if !t.events.is_empty()))
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    // First render stashes the list length; simulate by drawing once.
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            crate::view::working::draw(f, &app);
        })
        .unwrap();
    let len = app.trajectory_list_len.get();
    assert!(len > 0, "render must stash the list length");
    // Hammer Down past the end.
    for _ in 0..len + 5 {
        crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    assert_eq!(
        app.trajectory_cursor.get(),
        len - 1,
        "cursor must clamp to last row, not exceed it"
    );
}

#[test]
fn test_fmt_k_short_long() {
    assert_eq!(fmt_k(800), "800");
    assert_eq!(fmt_k(3200), "3.2k");
    assert_eq!(fmt_k(45200), "45.2k");
}

#[test]
fn test_pane_label_is_trajectory() {
    assert_eq!(crate::state::Pane::Trajectory.label(), "trajectory");
}

#[test]
fn test_level0_renders_turn_list() {
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            crate::view::working::draw(f, &app);
        })
        .unwrap();
}

/// A stub TrajectoryLog that flips a shared flag when called, so a test can
/// prove the render path read from the seam (not the mock fallback) when the
/// composition root wired an impl. The flag is shared via Arc so the test
/// reads it after the draw without downcasting the trait object.
struct StubLog {
    called: std::sync::Arc<std::sync::Mutex<bool>>,
}
impl TrajectoryLog for StubLog {
    fn trajectory(&self) -> TrajectoryView {
        *self.called.lock().unwrap() = true;
        TrajectoryView {
            session_id: "stub-session".into(),
            model: "stub-model".into(),
            total_turns: 0,
            tokens_in: None,
            tokens_out: None,
            failures: 0,
            duration_secs: 0,
            rows: Vec::new(),
        }
    }
}

#[test]
fn test_wired_seam_supplies_view() {
    // When the seam is Some, draw_content must call it (covering the Some
    // branch) rather than the mock fallback. The stub flips a shared flag on
    // call; a render pass leaves it set.
    let flag = std::sync::Arc::new(std::sync::Mutex::new(false));
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory_log = Some(std::sync::Arc::new(StubLog {
        called: flag.clone(),
    }));
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            crate::view::working::draw(f, &app);
        })
        .unwrap();
    assert!(
        *flag.lock().unwrap(),
        "draw_content must call the wired seam, not the mock fallback"
    );
}

#[test]
fn test_level1_renders_row_detail() {
    // Drilling a [bg] row (dream/compact/save) shows that row's detail at L1,
    // not the first turn's events. turn_idx 2 = the dream [bg] row in the mock.
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory_level.set(1);
    app.trajectory_turn_idx.set(2);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            crate::view::working::draw(f, &app);
        })
        .unwrap();
    assert!(
        app.trajectory_at_bg.get(),
        "L1 must flag the focused row as bg so Enter does not drill to L2"
    );
}

#[test]
fn test_level1_renders_turn_detail() {
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory_level.set(1);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            crate::view::working::draw(f, &app);
        })
        .unwrap();
}

#[test]
fn test_level2_renders_event_detail() {
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory_level.set(2);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            crate::view::working::draw(f, &app);
        })
        .unwrap();
}

/// L2 detail must render the content of EVERY real projection kind — not
/// just the mock kinds. Drives draw_event_detail with a view whose events use
/// the real kind strings (tool_call / tool_result / reasoning / llm) and
/// asserts the input / output / thinking text appears. This is the
/// regression guard for the kind-name-divergence trap: a kind-name match
/// table masked an empty L2 body for real sessions while the mock rendered
/// fine.
#[test]
fn test_level2_renders_projection_kinds() {
    use crate::view::trajectory_pane::{
        TrajectoryEvent, TrajectoryRow, TrajectoryTurn, TrajectoryView, draw_event_detail,
    };
    fn ev(
        kind: &str,
        thinking: Option<&str>,
        input: Option<&str>,
        output: Option<&str>,
    ) -> TrajectoryEvent {
        TrajectoryEvent {
            kind: kind.into(),
            summary: "preview".into(),
            start_ms: 0,
            duration_ms: 10,
            success: true,
            thinking: thinking.map(Into::into),
            input: input.map(Into::into),
            output: output.map(Into::into),
        }
    }
    let view = TrajectoryView {
        session_id: "s".into(),
        model: "m".into(),
        total_turns: 1,
        tokens_in: Some(0),
        tokens_out: Some(0),
        failures: 0,
        duration_secs: 0,
        rows: vec![TrajectoryRow::Turn(TrajectoryTurn {
            n: 1,
            user_input: "real kinds".into(),
            tokens_in: Some(0),
            tokens_out: Some(0),
            cache_read: Some(0),
            cache_write: Some(0),
            model: None,
            effort: None,
            reasoning_tokens: None,
            tool_count: 2,
            tool_fail: 0,
            retries: 0,
            duration_ms: 0,
            success: true,
            events: vec![
                ev("reasoning", Some("let me think"), None, None),
                ev("tool_call", None, Some("echo hi"), None),
                ev("tool_result", None, None, Some("hi")),
                ev("llm", Some("decided"), None, Some("the full reply")),
            ],
        })],
    };
    let body_text = |cursor: usize| {
        let (_, body, _) = draw_event_detail(&view, 0, cursor, ratatui::layout::Rect::ZERO);
        body.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
            .collect::<String>()
    };
    assert!(
        body_text(0).contains("let me think"),
        "reasoning shows its thinking at L2"
    );
    assert!(
        body_text(1).contains("echo hi"),
        "tool_call shows its input at L2"
    );
    assert!(
        body_text(2).contains("hi"),
        "tool_result shows its output at L2"
    );
    let llm = body_text(3);
    assert!(llm.contains("decided"), "llm shows thinking at L2");
    assert!(
        llm.contains("the full reply"),
        "llm shows the full reply at L2, not just the preview"
    );
}

/// Gantt-bar visual invariants on the mock trajectory at the turn-detail
/// level: unicode block bars render (█), the selection glyph pins the
/// focused row (▸), and the mock's content (a "cargo test" call) shows.
/// The mock is the only data with non-zero duration_ms (hardcoded in
/// mock_trajectory.rs); the real binary always wires a real SessionLog
/// whose fresh session has zero turns, so the bars are unreachable on the
/// real-binary PTY path — this unit test holds the bar invariants where
/// they are reachable, and dumps the rendered level to a temp file for
/// human visual review (the Gantt timeline, the cursor row, the unicode
/// bars vs ASCII hashes).
#[test]
fn test_trajectory_bar_invariants_mock() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.pane = crate::state::Pane::Trajectory;
    app.trajectory_level.set(1);
    let out = crate::test_support::render_text(&app, 100, 40);
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
    let path = std::env::temp_dir().join("houyi-trajectory-capture.txt");
    std::fs::write(&path, &out).unwrap();
}

/// Secrets in tool I/O must NOT render on the trajectory pane (a human-facing
/// surface — screen-share / recording / scrollback). The L2 event detail
/// redacts input/output/thinking before drawing; the durable log the pane
/// projects from stays full-fidelity. This pins the redact-on-read boundary.
#[test]
fn test_event_detail_redacts_secrets() {
    use crate::view::trajectory_pane::{
        TrajectoryEvent, TrajectoryRow, TrajectoryTurn, TrajectoryView, draw_event_detail,
    };
    let secret = "sk-abcd1234efgh5678ijkl9012mnop3456qrst";
    let view = TrajectoryView {
        session_id: "s".into(),
        model: "m".into(),
        total_turns: 1,
        tokens_in: Some(0),
        tokens_out: Some(0),
        failures: 0,
        duration_secs: 0,
        rows: vec![TrajectoryRow::Turn(TrajectoryTurn {
            n: 1,
            user_input: "show keys".into(),
            tokens_in: Some(0),
            tokens_out: Some(0),
            cache_read: Some(0),
            cache_write: Some(0),
            model: None,
            effort: None,
            reasoning_tokens: None,
            tool_count: 1,
            tool_fail: 0,
            retries: 0,
            duration_ms: 0,
            success: true,
            events: vec![TrajectoryEvent {
                kind: "tool_result".into(),
                summary: "creds".into(),
                start_ms: 0,
                duration_ms: 10,
                success: true,
                thinking: None,
                input: None,
                output: Some(format!("token={secret}")),
            }],
        })],
    };
    let (_, body, _) = draw_event_detail(&view, 0, 0, ratatui::layout::Rect::ZERO);
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    // Render once so the turn-list length is stashed — the drill guard reads
    // it to decide whether Enter may drill (it must not drill into an empty
    // row list). Without this render the stashed length is 0 and the guard
    // holds the pane at the turn-list level.
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| crate::view::working::draw(f, &app))
        .unwrap();
    assert_eq!(app.trajectory_level.get(), 0);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.trajectory_level.get(), 1);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.trajectory_level.get(), 2);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.trajectory_level.get(), 1);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.trajectory_level.get(), 0);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.pane, crate::state::Pane::Transcript);
}

#[test]
fn test_up_down_move_cursor() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Trajectory;
    assert_eq!(app.trajectory_cursor.get(), 0);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.trajectory_cursor.get(), 1);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.trajectory_cursor.get(), 0);
}

/// Thinking tokens render as (thinking Nk) only when Some and >0.
#[test]
fn test_thinking_tokens_render_nonzero() {
    use super::*;
    let view = TrajectoryView {
        session_id: "s".into(),
        model: "m".into(),
        total_turns: 1,
        tokens_in: Some(100),
        tokens_out: Some(50),
        failures: 0,
        duration_secs: 0,
        rows: vec![TrajectoryRow::Turn(TrajectoryTurn {
            n: 1,
            user_input: "hi".into(),
            tokens_in: Some(100),
            tokens_out: Some(50),
            cache_read: Some(0),
            cache_write: Some(0),
            model: None,
            effort: None,
            reasoning_tokens: Some(20),
            tool_count: 0,
            tool_fail: 0,
            retries: 0,
            duration_ms: 0,
            success: true,
            events: vec![],
        })],
    };
    let (_, body, _) = draw_turn_list(&view, 0, Rect::new(0, 0, 100, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("thinking"), "thinking shown when >0: {text}");
}

/// Thinking tokens are hidden when 0 or None.
#[test]
fn test_thinking_tokens_hidden_zero() {
    use super::*;
    let view = TrajectoryView {
        session_id: "s".into(),
        model: "m".into(),
        total_turns: 1,
        tokens_in: Some(100),
        tokens_out: Some(50),
        failures: 0,
        duration_secs: 0,
        rows: vec![TrajectoryRow::Turn(TrajectoryTurn {
            n: 1,
            user_input: "hi".into(),
            tokens_in: Some(100),
            tokens_out: Some(50),
            cache_read: Some(0),
            cache_write: Some(0),
            model: None,
            effort: None,
            reasoning_tokens: None,
            tool_count: 0,
            tool_fail: 0,
            retries: 0,
            duration_ms: 0,
            success: true,
            events: vec![],
        })],
    };
    let (_, body, _) = draw_turn_list(&view, 0, Rect::new(0, 0, 100, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        !text.contains("thinking"),
        "thinking hidden when None: {text}"
    );
}

/// Per-turn model renders only when ≥2 distinct models in the session.
#[test]
fn test_per_turn_model_two() {
    use super::*;
    let view = TrajectoryView {
        session_id: "s".into(),
        model: "2 models".into(),
        total_turns: 2,
        tokens_in: Some(200),
        tokens_out: Some(100),
        failures: 0,
        duration_secs: 0,
        rows: vec![
            TrajectoryRow::Turn(TrajectoryTurn {
                n: 1,
                user_input: "a".into(),
                tokens_in: Some(100),
                tokens_out: Some(50),
                cache_read: Some(0),
                cache_write: Some(0),
                model: Some("qwen3.7-max".into()),
                effort: Some("high".into()),
                reasoning_tokens: None,
                tool_count: 0,
                tool_fail: 0,
                retries: 0,
                duration_ms: 0,
                success: true,
                events: vec![],
            }),
            TrajectoryRow::Turn(TrajectoryTurn {
                n: 2,
                user_input: "b".into(),
                tokens_in: Some(100),
                tokens_out: Some(50),
                cache_read: Some(0),
                cache_write: Some(0),
                model: Some("glm-5.2".into()),
                effort: None,
                reasoning_tokens: None,
                tool_count: 0,
                tool_fail: 0,
                retries: 0,
                duration_ms: 0,
                success: true,
                events: vec![],
            }),
        ],
    };
    let (_, body, _) = draw_turn_list(&view, 0, Rect::new(0, 0, 100, 20));
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
    use super::*;
    let view = TrajectoryView {
        session_id: "s".into(),
        model: "qwen3.7-max".into(),
        total_turns: 2,
        tokens_in: Some(200),
        tokens_out: Some(100),
        failures: 0,
        duration_secs: 0,
        rows: vec![
            TrajectoryRow::Turn(TrajectoryTurn {
                n: 1,
                user_input: "a".into(),
                tokens_in: Some(100),
                tokens_out: Some(50),
                cache_read: Some(0),
                cache_write: Some(0),
                model: Some("qwen3.7-max".into()),
                effort: None,
                reasoning_tokens: None,
                tool_count: 0,
                tool_fail: 0,
                retries: 0,
                duration_ms: 0,
                success: true,
                events: vec![],
            }),
            TrajectoryRow::Turn(TrajectoryTurn {
                n: 2,
                user_input: "b".into(),
                tokens_in: Some(100),
                tokens_out: Some(50),
                cache_read: Some(0),
                cache_write: Some(0),
                model: Some("qwen3.7-max".into()),
                effort: None,
                reasoning_tokens: None,
                tool_count: 0,
                tool_fail: 0,
                retries: 0,
                duration_ms: 0,
                success: true,
                events: vec![],
            }),
        ],
    };
    let (_, body, _) = draw_turn_list(&view, 0, Rect::new(0, 0, 100, 20));
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

/// A turn with a user input uses it as the title.
#[test]
fn test_turn_title_user_input() {
    let turn = TrajectoryTurn {
        n: 1,
        user_input: "fix the bug".into(),
        tokens_in: None,
        tokens_out: None,
        cache_read: None,
        cache_write: None,
        model: None,
        effort: None,
        reasoning_tokens: None,
        tool_count: 0,
        tool_fail: 0,
        retries: 0,
        duration_ms: 0,
        success: true,
        events: vec![],
    };
    assert_eq!(turn_title(&turn), "fix the bug");
}

/// A turn with no user input (a tool-continuation turn) derives a fallback
/// title from the first event's summary so the row is not blank.
#[test]
fn test_turn_title_falls_back() {
    let turn = TrajectoryTurn {
        n: 2,
        user_input: String::new(),
        tokens_in: None,
        tokens_out: None,
        cache_read: None,
        cache_write: None,
        model: None,
        effort: None,
        reasoning_tokens: None,
        tool_count: 1,
        tool_fail: 0,
        retries: 0,
        duration_ms: 0,
        success: true,
        events: vec![TrajectoryEvent {
            kind: "tool_call".into(),
            summary: "Bash: ls -la".into(),
            start_ms: 0,
            duration_ms: 10,
            success: true,
            thinking: None,
            input: None,
            output: None,
        }],
    };
    let title = turn_title(&turn);
    assert!(
        title.contains("(continued)") && title.contains("Bash: ls -la"),
        "fallback title must mark continuation + carry the event summary: {title}"
    );
}

/// A turn with no user input and no events shows "(no input)" — never blank.
#[test]
fn test_turn_title_empty() {
    let turn = TrajectoryTurn {
        n: 3,
        user_input: String::new(),
        tokens_in: None,
        tokens_out: None,
        cache_read: None,
        cache_write: None,
        model: None,
        effort: None,
        reasoning_tokens: None,
        tool_count: 0,
        tool_fail: 0,
        retries: 0,
        duration_ms: 0,
        success: true,
        events: vec![],
    };
    assert_eq!(turn_title(&turn), "(no input)");
}
