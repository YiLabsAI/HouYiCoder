//! Regression tests for drag-selection bugs: a completed drag must not feed
//! the multi-click chain, edge auto-scroll during a drag must move one line
//! (not a page), and the highlight plus copy must track content rows when
//! the viewport shifts between mouse events (tail-follow append).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::style::Color;
use std::sync::{Arc, Mutex};

use crate::composition;
use crate::selection::RecordingClipboard;
use crate::state::Screen;
use crate::test_support::{render_buffer, render_text};

/// App with n numbered system lines, rendered once so rect/rows/scroll cells
/// are published. Returns the app plus the clipboard capture handle.
fn app_with_lines(n: usize) -> (crate::state::App, Arc<Mutex<Vec<String>>>) {
    let mut app = composition::app();
    app.screen = Screen::Working;
    for i in 0..n {
        app.system_line(format!("history line {i:02}"));
    }
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    app.clipboard = Arc::new(RecordingClipboard {
        captured: captured.clone(),
    });
    let _out = render_text(&app, 80, 24);
    (app, captured)
}

/// Screen row of the all-rows index under the current scroll offset.
fn screen_y(app: &crate::state::App, ri: usize) -> u16 {
    let rect = app.transcript_rect.get();
    let total = app.transcript_scroll.total.get();
    let top = app.transcript_scroll.top_offset(total);
    rect.y + (ri.saturating_sub(top)) as u16
}

/// Index into last_all_rows of the row containing the needle.
fn row_index(app: &crate::state::App, needle: &str) -> usize {
    app.last_all_rows
        .borrow()
        .iter()
        .position(|(_, s)| s.contains(needle))
        .expect("needle row present")
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

/// A full drag gesture, then an immediate second press at the same cell:
/// the second gesture must start a fresh char-mode selection, not escalate
/// to word-select via the multi-click chain (the user report: repeated drag
/// attempts select whole words/lines they never asked for).
#[test]
fn test_redrag_stays_char_mode() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript = vec![crate::records::TranscriptLine::Agent(
        "alpha bravo charlie".into(),
    )];
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    app.clipboard = Arc::new(RecordingClipboard {
        captured: captured.clone(),
    });
    let _out = render_text(&app, 80, 24);
    let rect = app.transcript_rect.get();
    let y = screen_y(&app, row_index(&app, "alpha"));
    let x0 = rect.x + 8;
    // First gesture: press, drag right, release. A real drag.
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), x0, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), x0 + 4, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Up(MouseButton::Left), x0 + 4, y),
    );
    // Second gesture right away at the same cell (within the multi-click
    // window). It must be a fresh char-mode drag: no word/line span, anchor
    // exactly at the press column.
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), x0, y),
    );
    assert!(
        app.selection.span_origin.is_none(),
        "press after a completed drag must not escalate to word-select"
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), x0 + 2, y),
    );
    let (ax, _) = app.selection.anchor.expect("anchor set");
    assert_eq!(
        ax, x0,
        "char-mode anchor must sit at the press column, not a word bound"
    );
}

/// Dragging onto the top edge row of the transcript must scroll exactly one
/// line and keep anchor/focus content rows consistent with the new offset.
/// The old behavior paged a full viewport mid-drag and mapped the focus with
/// the pre-scroll offset (selection jumped wildly).
#[test]
fn test_edge_drag_scrolls_line() {
    let (mut app, _captured) = app_with_lines(40);
    let rect = app.transcript_rect.get();
    app.transcript_scroll.follow_tail = false;
    app.transcript_scroll.offset = 10;
    let _out = render_text(&app, 80, 24);
    let total = app.transcript_scroll.total.get();
    assert_eq!(app.transcript_scroll.top_offset(total), 10);
    // Press mid-viewport, then drag onto the top edge row.
    crate::app::handle_mouse(
        &mut app,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            rect.x + 2,
            rect.y + 3,
        ),
    );
    assert_eq!(app.selection.anchor.map(|(_, r)| r), Some(13));
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), rect.x + 2, rect.y),
    );
    let after = app.transcript_scroll.top_offset(total);
    assert_eq!(after, 9, "edge drag must scroll one line, not a page");
    assert_eq!(
        app.selection.anchor.map(|(_, r)| r),
        Some(13),
        "anchor content row is fixed for the whole drag"
    );
    assert_eq!(
        app.selection.focus.map(|(_, r)| r),
        Some(9),
        "focus content row must map through the post-scroll offset"
    );
}

/// A finished selection must keep highlighting the same content rows when
/// the tail-follow viewport shifts under it (agent appends lines between
/// mouse events). The old overlay painted cached screen rows, leaving the
/// highlight on whatever text scrolled into those rows.
#[test]
fn test_overlay_tracks_shifted_content() {
    let (mut app, _captured) = app_with_lines(40);
    let rect = app.transcript_rect.get();
    let ri = row_index(&app, "history line 35");
    let y = screen_y(&app, ri);
    assert!(
        y > rect.y && y < rect.y + rect.height.saturating_sub(1),
        "target row must be visible and off the scroll edges (y={y})"
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), rect.x + 6, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Up(MouseButton::Left), rect.x + 6, y),
    );
    // Viewport shifts up by two rows while the selection is held.
    app.system_line("fresh tail line A");
    app.system_line("fresh tail line B");
    let buf = render_buffer(&app, 80, 24);
    let y_new = screen_y(&app, ri);
    assert_ne!(y, y_new, "append must shift the target row on screen");
    let sel_bg = Color::Indexed(24);
    let cell = buf.cell((rect.x + 1, y_new)).expect("target cell");
    assert_eq!(
        cell.bg, sel_bg,
        "highlight must follow the selected content row to its new screen row"
    );
    let stale = buf.cell((rect.x + 1, y)).expect("stale cell");
    assert_ne!(
        stale.bg, sel_bg,
        "the old screen row now shows different text and must not stay highlighted"
    );
}

/// Ctrl+C after the viewport shifted must copy the rows that were selected,
/// not the rows that scrolled into the old screen position. The old
/// extraction derived the focus row from the stale screen y plus the new
/// scroll offset, widening the copy onto unrelated rows.
#[test]
fn test_copy_tracks_shifted_content() {
    let (mut app, captured) = app_with_lines(40);
    let rect = app.transcript_rect.get();
    let ri = row_index(&app, "history line 35");
    let y = screen_y(&app, ri);
    assert!(
        y > rect.y && y < rect.y + rect.height.saturating_sub(1),
        "target row must be visible and off the scroll edges (y={y})"
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), rect.x + 20, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Up(MouseButton::Left), rect.x + 20, y),
    );
    let first = captured.lock().expect("captured").clone();
    assert_eq!(first.len(), 1, "one copy on release: {first:?}");
    assert!(first[0].contains("history line 35"), "got: {first:?}");
    // Viewport shifts, then an explicit Ctrl+C re-copy of the held selection.
    app.system_line("fresh tail line A");
    app.system_line("fresh tail line B");
    let _out = render_text(&app, 80, 24);
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    );
    let got = captured.lock().expect("captured").clone();
    assert_eq!(got.len(), 2, "ctrl+c copies again: {got:?}");
    assert_eq!(
        got[1], first[0],
        "re-copy must return the originally selected row, not shifted rows"
    );
}

/// Regression: clicking ONE ThoughtFor row must expand ONLY that one's
/// reasoning, not every ThoughtFor in the transcript. The hint text says
/// "ctrl+o to expand" but a click also toggles (both inputs expand/collapse);
/// the bug was that one click expanded all of them. Set up two ThoughtFor
/// rows with distinct reasoning, click the first, and assert only the
/// first's reasoning key lands in expanded_thinking.
#[test]
fn test_click_thoughtfor_expands_one() {
    use crate::records::TranscriptLine;
    let mut app = composition::app();
    app.screen = Screen::Working;
    let r1 = "reasoning for turn one".to_string();
    let r2 = "reasoning for turn two".to_string();
    app.transcript = vec![
        TranscriptLine::User("q1".into()),
        TranscriptLine::Agent("a1".into()),
        TranscriptLine::ThoughtFor {
            secs: 2,
            reasoning: Some(r1.clone()),
            tool_summary: None,
            turn_id: "t1".into(),
        },
        TranscriptLine::User("q2".into()),
        TranscriptLine::Agent("a2".into()),
        TranscriptLine::ThoughtFor {
            secs: 5,
            reasoning: Some(r2.clone()),
            tool_summary: None,
            turn_id: "t2".into(),
        },
    ];
    let _out = render_text(&app, 80, 24);
    let rect = app.transcript_rect.get();
    // Find the FIRST ThoughtFor row's visible index + screen y.
    let rows = app.last_transcript_rows.borrow();
    let ri = rows
        .iter()
        .position(|(_, t)| t.contains("Thought for 2s"))
        .expect("first ThoughtFor row rendered");
    drop(rows);
    let y = rect.y + ri as u16;
    let x = rect.x;
    // Click (Down + Up) the first ThoughtFor row.
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), x, y),
    );
    crate::app::handle_mouse(&mut app, mouse(MouseEventKind::Up(MouseButton::Left), x, y));
    assert_eq!(
        app.expanded_thinking.len(),
        1,
        "clicking one ThoughtFor must expand only one, got: {:?}",
        app.expanded_thinking
    );
    assert!(
        app.expanded_thinking.contains("t1"),
        "the clicked row's turn_id should be expanded: {:?}",
        app.expanded_thinking
    );
    assert!(
        !app.expanded_thinking.contains("t2"),
        "the other row's turn_id must NOT expand: {:?}",
        app.expanded_thinking
    );
}

/// Regression: a ThoughtFor with reasoning=None (rendered without the
/// "(ctrl+o to ...)" hint) before one with reasoning=Some must not break
/// click-expand on the Some row. The click-path counter used to count ALL
/// "Thought for" rows (incl. None) while the transcript scan only iterated
/// reasoning: Some — the mismatch made the click return false + swallow the
/// gesture. Now the counter skips None rows (they carry no "ctrl+o" hint).
#[test]
fn test_click_thoughtfor_none_reasoning() {
    use crate::records::TranscriptLine;
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript = vec![
        TranscriptLine::User("q1".into()),
        TranscriptLine::Agent("a1".into()),
        // No reasoning: renders as "Thought for 3s" with no (ctrl+o) hint.
        TranscriptLine::ThoughtFor {
            secs: 3,
            reasoning: None,
            tool_summary: None,
            turn_id: "t1".into(),
        },
        TranscriptLine::User("q2".into()),
        TranscriptLine::Agent("a2".into()),
        // Has reasoning: renders with "(ctrl+o to expand)".
        TranscriptLine::ThoughtFor {
            secs: 5,
            reasoning: Some("real reasoning for turn two".into()),
            tool_summary: None,
            turn_id: "t2".into(),
        },
    ];
    let _out = render_text(&app, 80, 24);
    let rect = app.transcript_rect.get();
    let rows = app.last_transcript_rows.borrow();
    // The Some row's header carries "(ctrl+o to expand)"; the None row does not.
    let ri = rows
        .iter()
        .position(|(_, t)| t.contains("Thought for 5s") && t.contains("ctrl+o"))
        .expect("the reasoning=Some ThoughtFor row rendered with a hint");
    drop(rows);
    let y = rect.y + ri as u16;
    let x = rect.x;
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), x, y),
    );
    crate::app::handle_mouse(&mut app, mouse(MouseEventKind::Up(MouseButton::Left), x, y));
    assert!(
        app.expanded_thinking.contains("t2"),
        "clicking the reasoning=Some ThoughtFor (after a None one) must expand it: {:?}",
        app.expanded_thinking
    );
    assert!(
        !app.expanded_thinking.contains("t1"),
        "the None-reasoning row has nothing to expand: {:?}",
        app.expanded_thinking
    );
}

/// Regression: when an earlier ThoughtFor (reasoning=Some) has scrolled OFF
/// the top of the viewport and a later one is visible, clicking the visible
/// one must expand IT, not the off-screen one. The old click path counted
/// "Nth visible expandable ThoughtFor" then matched it to "Nth in the FULL
/// transcript" — with t1 off-screen the visible count was 1 (only t2) but the
/// full-transcript count matched t1 (the first), so a click on the visible
/// bottom row expanded the off-screen top row ("point at bottom, expand at
/// top"). The fix publishes last_row_turn_ids so the click resolves straight
/// to the visible row's turn_id.
#[test]
fn test_click_thoughtfor_scrolled_off() {
    use crate::records::TranscriptLine;
    let mut app = composition::app();
    app.screen = Screen::Working;
    let mut transcript = vec![
        TranscriptLine::User("q1".into()),
        TranscriptLine::Agent("a1".into()),
        TranscriptLine::ThoughtFor {
            secs: 2,
            reasoning: Some("off-screen reasoning turn one".into()),
            tool_summary: None,
            turn_id: "t1".into(),
        },
    ];
    // Padding so t1 scrolls off the top when the viewport shows the tail.
    for _ in 0..15 {
        transcript.push(TranscriptLine::Agent(
            "filler line to push t1 off-screen".into(),
        ));
    }
    transcript.push(TranscriptLine::ThoughtFor {
        secs: 5,
        reasoning: Some("visible reasoning turn two".into()),
        tool_summary: None,
        turn_id: "t2".into(),
    });
    app.transcript = transcript;
    // Small height so the tail viewport shows only t2 (t1 scrolled off top).
    let _out = render_text(&app, 80, 8);
    let rect = app.transcript_rect.get();
    let rows = app.last_transcript_rows.borrow();
    let ri = rows
        .iter()
        .position(|(_, t)| t.contains("Thought for 5s") && t.contains("ctrl+o"))
        .expect("the visible (tail) ThoughtFor t2 rendered with a hint");
    // t1 must NOT be in the visible rows (it scrolled off the top).
    assert!(
        !rows.iter().any(|(_, t)| t.contains("Thought for 2s")),
        "t1 should be scrolled off the viewport: {:?}",
        rows.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>()
    );
    drop(rows);
    let y = rect.y + ri as u16;
    let x = rect.x;
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), x, y),
    );
    crate::app::handle_mouse(&mut app, mouse(MouseEventKind::Up(MouseButton::Left), x, y));
    assert!(
        app.expanded_thinking.contains("t2"),
        "clicking the visible t2 must expand t2: {:?}",
        app.expanded_thinking
    );
    assert!(
        !app.expanded_thinking.contains("t1"),
        "the off-screen t1 must NOT expand from a click on t2: {:?}",
        app.expanded_thinking
    );
}

/// Regression: two ThoughtFor rows with the SAME reasoning text (two turns
/// happened to produce identical reasoning) must still expand independently.
/// The bug: expanded_thinking is keyed by reasoning text, so clicking one
/// expanded BOTH (the shared key made both rows' render check true). Keying
/// by reasoning text collides; each ThoughtFor needs its own identity.
#[test]
fn test_same_reason_thoughts_independent() {
    use crate::records::TranscriptLine;
    let mut app = composition::app();
    app.screen = Screen::Working;
    let shared = "the model reasoned the same way twice".to_string();
    app.transcript = vec![
        TranscriptLine::User("q1".into()),
        TranscriptLine::Agent("a1".into()),
        TranscriptLine::ThoughtFor {
            secs: 2,
            reasoning: Some(shared.clone()),
            tool_summary: None,
            turn_id: "t1".into(),
        },
        TranscriptLine::User("q2".into()),
        TranscriptLine::Agent("a2".into()),
        TranscriptLine::ThoughtFor {
            secs: 5,
            reasoning: Some(shared.clone()),
            tool_summary: None,
            turn_id: "t2".into(),
        },
    ];
    let _out = render_text(&app, 80, 24);
    let rect = app.transcript_rect.get();
    let rows = app.last_transcript_rows.borrow();
    let ri = rows
        .iter()
        .position(|(_, t)| t.contains("Thought for 2s"))
        .expect("first ThoughtFor row rendered");
    drop(rows);
    let y = rect.y + ri as u16;
    let x = rect.x;
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), x, y),
    );
    crate::app::handle_mouse(&mut app, mouse(MouseEventKind::Up(MouseButton::Left), x, y));
    // After clicking the FIRST ThoughtFor, only the first should show
    // expanded. Render + assert the second's hint still says "expand".
    let out = render_text(&app, 80, 24);
    let first_hint = out.matches("Thought for 2s (ctrl+o to collapse)").count();
    let second_expand_hint = out.matches("Thought for 5s (ctrl+o to expand)").count();
    assert_eq!(
        first_hint, 1,
        "first ThoughtFor should be expanded (collapse hint): {out}"
    );
    assert_eq!(
        second_expand_hint, 1,
        "second ThoughtFor must stay collapsed (expand hint), not expand too: {out}"
    );
}

/// Helper: an app whose transcript has a foldable two-call group that
/// collapses to one summary. Returns the app rendered once (collapsed state).
fn app_with_fold_group() -> crate::state::App {
    use crate::records::{ToolOutcome, TranscriptLine};
    let mut app = composition::app();
    app.screen = Screen::Working;
    let tcall = |cid: &str, name: &str, brief: &str, oc: ToolOutcome| TranscriptLine::Tool {
        name: name.to_string(),
        tool: name.to_string(),
        status: brief.to_string(),
        invocation: brief.to_string(),
        outcome: oc,
        call_id: cid.to_string(),
        body: String::new(),
        is_diff: false,
    };
    let tresult = |cid: &str, body: &str, oc: ToolOutcome| TranscriptLine::Tool {
        name: "result".to_string(),
        tool: "result".to_string(),
        status: String::new(),
        invocation: String::new(),
        outcome: oc,
        call_id: cid.to_string(),
        body: body.to_string(),
        is_diff: false,
    };
    app.transcript = vec![
        TranscriptLine::User("hi".to_string()),
        tcall("c1", "bash", "ls -la", ToolOutcome::Success),
        tresult("c1", "done", ToolOutcome::Success),
        tcall("c2", "read", "a.rs", ToolOutcome::Success),
        tresult("c2", "content", ToolOutcome::Success),
        TranscriptLine::Agent("all done".into()),
    ];
    let _out = render_text(&app, 80, 24);
    app
}

/// Expand the first fold group by clicking its collapsed summary row.
/// Returns the group key ("c1#0") and re-renders so body rows are published.
fn expand_first_group(app: &mut crate::state::App) -> String {
    let fold_ri = app
        .last_row_fold_keys
        .borrow()
        .iter()
        .position(|k| k.is_some())
        .expect("a fold summary row exists");
    let rect = app.transcript_rect.get();
    let y = screen_y(app, fold_ri_to_all_rows_index(app, fold_ri));
    crate::app::handle_mouse(
        app,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x, y),
    );
    let _out = render_text(app, 80, 24);
    "c1#0".to_string()
}

/// Map a visible fold-row index (into last_row_fold_keys) back to its
/// last_all_rows index so screen_y can place it on screen.
fn fold_ri_to_all_rows_index(app: &crate::state::App, fold_ri: usize) -> usize {
    let total = app.transcript_scroll.total.get();
    let top = app.transcript_scroll.top_offset(total);
    top + fold_ri
}

/// A clean click on a BODY row inside an expanded fold block collapses the
/// whole group (click anywhere in the expanded region collapses — an
/// editor-style expanded-block click target). The body row is NOT the
/// summary header, so the Down press starts a selection; the clean release
/// (no drag motion) flips to collapse instead of copy.
#[test]
fn test_click_body_collapses_group() {
    let mut app = app_with_fold_group();
    let key = expand_first_group(&mut app);
    assert!(
        app.expanded_fold_groups.contains(&key),
        "group should be expanded after summary click"
    );
    // A body row (the bash call line) — NOT the summary header.
    let body_ri = row_index(&app, "Bash(ls");
    let rect = app.transcript_rect.get();
    let y = screen_y(&app, body_ri);
    // Clean click: Down + Up at the same cell (no drag motion).
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Up(MouseButton::Left), rect.x, y),
    );
    assert!(
        !app.expanded_fold_groups.contains(&key),
        "clean click on an expanded body row must collapse the group"
    );
}

/// A drag inside an expanded fold block selects + copies text and must NOT
/// collapse the group. Drag-select inside an
/// expanded block copies (the terminal selects natively under no mouse
/// capture; we own selection under mouse capture, so drag wins over
/// collapse).
#[test]
fn test_drag_expanded_block_copies() {
    let mut app = app_with_fold_group();
    // Recording clipboard so we can assert a copy fired.
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    app.clipboard = Arc::new(RecordingClipboard {
        captured: captured.clone(),
    });
    let key = expand_first_group(&mut app);
    let body_ri = row_index(&app, "Bash(ls");
    let rect = app.transcript_rect.get();
    let y = screen_y(&app, body_ri);
    // A real drag: press, move right, release at a different column.
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), rect.x + 6, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Up(MouseButton::Left), rect.x + 6, y),
    );
    let copied = captured.lock().expect("captured").clone();
    assert!(
        !copied.is_empty(),
        "drag in expanded block must copy (drag wins over collapse): {copied:?}"
    );
    assert!(
        app.expanded_fold_groups.contains(&key),
        "drag must NOT collapse the expanded group"
    );
}

/// A drag that leaves the start cell and returns to it (anchor == focus
/// again, but drag_moved is true) must NOT collapse the expanded group. The
/// collapse guard requires a true clean click (no drag motion); a returned
/// drag falls through to finish_drag's clear path, matching the existing
/// click-only clear semantics.
#[test]
fn test_drag_return_keeps_group() {
    let mut app = app_with_fold_group();
    let key = expand_first_group(&mut app);
    let body_ri = row_index(&app, "Bash(ls");
    let rect = app.transcript_rect.get();
    let y = screen_y(&app, body_ri);
    let x0 = rect.x + 2;
    // Press on the body row, drag right, drag back to the start cell, release.
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), x0, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), x0 + 4, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), x0, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Up(MouseButton::Left), x0, y),
    );
    assert!(
        app.expanded_fold_groups.contains(&key),
        "a drag that returned to its start must not collapse the group"
    );
}

/// The expanded fold block paints a gray bg over its body rows so the whole
/// region reads as one collapsible affordance (an editor-style
/// expanded-block selection region). A collapsed summary row keeps
/// the dim style — it is NOT part of an expanded region.
#[test]
fn test_expanded_block_paints_gray() {
    let mut app = app_with_fold_group();
    // Collapsed: the summary row is dim, NOT gray.
    let summary_ri = app
        .last_row_fold_keys
        .borrow()
        .iter()
        .position(|k| k.is_some())
        .expect("collapsed summary row exists");
    let buf = render_buffer(&app, 80, 24);
    let summary_y = screen_y(&app, summary_ri);
    let gray = Color::Indexed(238);
    let summary_cell = buf
        .cell((app.transcript_rect.get().x + 1, summary_y))
        .expect("summary cell");
    assert_ne!(
        summary_cell.bg, gray,
        "collapsed summary must not carry the expanded-block gray bg"
    );
    // Expanded: body rows carry the gray bg.
    expand_first_group(&mut app);
    let body_ri = row_index(&app, "Bash(ls");
    let buf = render_buffer(&app, 80, 24);
    let body_y = screen_y(&app, body_ri);
    let body_cell = buf
        .cell((app.transcript_rect.get().x + 2, body_y))
        .expect("body cell");
    assert_eq!(
        body_cell.bg, gray,
        "expanded-block body row must carry the gray bg"
    );
}
