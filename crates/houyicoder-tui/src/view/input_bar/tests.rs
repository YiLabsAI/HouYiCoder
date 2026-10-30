//! Render tests for the input bar's direct-shell mode.

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::draw_input;
use crate::composition;
use crate::input::InputMode;
use crate::state::Screen;

/// DirectShell mode paints a cyan border with the sandboxed title and a dollar
/// glyph, so the user can tell the mode apart from chat at a glance.
#[test]
fn test_draw_shell_mode_title() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.input.mode = InputMode::DirectShell;
    app.ui.input.editor.set("cargo test".to_string());
    let backend = TestBackend::new(60, 5);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            draw_input(f, area, &app);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
    assert!(
        text.contains("bash · sandboxed"),
        "the border title names the sandboxed shell mode: {text}"
    );
    assert!(text.contains("$ "), "the prompt glyph is a dollar: {text}");
}
