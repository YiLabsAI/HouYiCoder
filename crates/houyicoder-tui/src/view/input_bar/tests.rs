//! Render tests for the input bar's direct-shell mode.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

use super::draw_input;
use crate::composition;
use crate::input::InputMode;
use crate::state::Screen;

/// Draw the input bar in DirectShell mode and return its buffer text and the
/// colors its cells paint.
fn draw_shell(label: &str) -> (String, Vec<Color>) {
    let mut app = composition::app();
    app.sessions.current.sandbox = label.to_string();
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
    let colors: Vec<Color> = buffer.content().iter().map(|c| c.fg).collect();
    (text, colors)
}

/// DirectShell mode paints a cyan border whose title names the sandbox label
/// the session was composed with, and a dollar glyph, so the user can tell the
/// mode apart from chat at a glance.
#[test]
fn test_draw_shell_mode_title() {
    let (text, _) = draw_shell("mac-seatbelt");
    assert!(
        text.contains("bash · mac-seatbelt"),
        "the border title names the composed sandbox: {text}"
    );
    assert!(text.contains("$ "), "the prompt glyph is a dollar: {text}");
}

/// A session with no fence says so in the title, in the warning color: the
/// title must never claim a boundary the session does not have. The local
/// login writes the label with a qualifier, so an off-prefixed label counts as
/// off rather than falling through to a sandboxed claim.
#[test]
fn test_unfenced_title_warns() {
    for label in ["off", "off (local)"] {
        let (text, colors) = draw_shell(label);
        assert!(
            text.contains("bash · unfenced"),
            "the title reports the absent fence for {label}: {text}"
        );
        assert!(
            !text.contains("bash · sandboxed"),
            "the title does not claim a fence for {label}: {text}"
        );
        assert!(
            colors.contains(&Color::Yellow),
            "the unfenced title for {label} is painted in the warning color"
        );
    }
}
