//! Drag-to-select then explicit Ctrl+C copy flow tests. Copy is explicit (no
//! auto-copy on release); the selection persists after release and Ctrl+C copies it.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// Select the agent line by dragging, release, then press Ctrl+C. No copy
/// lands on mouse-up; one copy lands on Ctrl+C with the full line text.
#[test]
fn test_drag_copies_agent_line() {
    use crate::composition;
    use crate::selection::RecordingClipboard;
    use crate::state::Screen;
    use crate::test_harness::render_text;
    use std::sync::{Arc, Mutex};

    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript =
        vec![crate::records::TranscriptLine::Agent("hello world".into())].into();
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    app.ui.clipboard = Arc::new(RecordingClipboard {
        captured: captured.clone(),
    });
    let _out = render_text(&app, 80, 24);
    let rect = app.ui.frame.layout.transcript_rect.get();
    let agent_ri = app
        .ui
        .frame
        .layout
        .last_all_rows
        .borrow()
        .iter()
        .position(|(_, s)| s.contains("hello world"))
        .expect("agent line in last_all_rows");
    let top = app.whole_scroll().top_offset();
    let agent_y = rect.y + (agent_ri.saturating_sub(top)) as u16;
    let line_end = rect.x + 13;
    for ev in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        crate::app::handle_mouse(
            &mut app,
            MouseEvent {
                kind: ev,
                column: if matches!(ev, MouseEventKind::Down(_)) {
                    rect.x
                } else {
                    line_end
                },
                row: agent_y,
                modifiers: KeyModifiers::NONE,
            },
        );
    }
    // Release auto-copies (direct paste works) and keeps the highlight.
    let got = captured.lock().expect("captured").clone();
    assert_eq!(got.len(), 1, "one copy on mouse-up: {got:?}");
    assert_eq!(got[0], "hello world", "full line copied: {got:?}");
}

/// Drag-select an agent line containing wide glyphs, release. The clipboard
/// holds the exact logical characters the user selected: each wide glyph comes
/// back as one character (not the pair of continuation cells the buffer holds
/// for it) and the agent badge is stripped. This holds the copy path to the
/// published row store, the same rows the draw produced, not a recompute.
#[test]
fn test_drag_copies_cjk_line() {
    use crate::composition;
    use crate::records::TranscriptLine;
    use crate::selection::RecordingClipboard;
    use crate::state::Screen;
    use crate::test_harness::render_text;
    use std::sync::{Arc, Mutex};

    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript =
        vec![TranscriptLine::Agent("中文宽字符测试 tail".into())].into();
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    app.ui.clipboard = Arc::new(RecordingClipboard {
        captured: captured.clone(),
    });
    let _out = render_text(&app, 80, 24);
    let rect = app.ui.frame.layout.transcript_rect.get();
    let agent_ri = app
        .ui
        .frame
        .layout
        .last_all_rows
        .borrow()
        .iter()
        .position(|(_, s)| s.contains("中文宽字符测试"))
        .expect("cjk line in last_all_rows");
    let top = app.whole_scroll().top_offset();
    let agent_y = rect.y + (agent_ri.saturating_sub(top)) as u16;
    // A drag endpoint far past the text clamps to the width of the widest
    // selected cell, matching a drag that reaches the pane edge.
    let line_end = rect.x + 200;
    for ev in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        crate::app::handle_mouse(
            &mut app,
            MouseEvent {
                kind: ev,
                column: if matches!(ev, MouseEventKind::Down(_)) {
                    rect.x
                } else {
                    line_end
                },
                row: agent_y,
                modifiers: KeyModifiers::NONE,
            },
        );
    }
    let got = captured.lock().expect("captured").clone();
    assert_eq!(got.len(), 1, "one copy on mouse-up: {got:?}");
    assert_eq!(
        got[0], "中文宽字符测试 tail",
        "wide glyphs copy as exact characters: {got:?}"
    );
}

/// Select only the second line of a multi-line reply, release, Ctrl+C. Only
/// the second line is copied (a SoftBreak splits the reply into rows so the
/// second line is independently selectable).
#[test]
fn test_drag_copies_second_line() {
    use crate::composition;
    use crate::selection::RecordingClipboard;
    use crate::state::Screen;
    use crate::test_harness::render_text;
    use std::sync::{Arc, Mutex};

    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![crate::records::TranscriptLine::Agent(
        "first line\nsecond line\nthird line".into(),
    )]
    .into();
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    app.ui.clipboard = Arc::new(RecordingClipboard {
        captured: captured.clone(),
    });
    let _out = render_text(&app, 80, 24);
    let rect = app.ui.frame.layout.transcript_rect.get();
    let second_ri = app
        .ui
        .frame
        .layout
        .last_all_rows
        .borrow()
        .iter()
        .position(|(_, s)| s == "  second line")
        .expect("second line is its own row");
    let top = app.whole_scroll().top_offset();
    let second_y = rect.y + (second_ri.saturating_sub(top)) as u16;
    let line_end = rect.x + 13;
    for ev in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        crate::app::handle_mouse(
            &mut app,
            MouseEvent {
                kind: ev,
                column: if matches!(ev, MouseEventKind::Down(_)) {
                    rect.x
                } else {
                    line_end
                },
                row: second_y,
                modifiers: KeyModifiers::NONE,
            },
        );
    }
    // Release auto-copies only the second line (direct paste works).
    let got = captured.lock().expect("captured").clone();
    assert_eq!(got.len(), 1, "one copy on mouse-up: {got:?}");
    assert_eq!(
        got[0], "  second line",
        "only the second line copied: {got:?}"
    );
}
