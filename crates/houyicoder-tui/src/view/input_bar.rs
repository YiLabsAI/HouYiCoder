//! The input editor and the inline Ctrl+F search bar. Extracted from
//! working.rs so the working-surface file owns layout, not editor chrome.
//! Working mode is the only viewport that shows the input box (Focus and
//! Scroll hide it), so the editor lives here, next to the layout that
//! reserves its row.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::state::App;

/// Render the inline Ctrl+F search bar (one line below the input box).
pub(super) fn draw_inline_search(f: &mut Frame, area: Rect, app: &App) {
    let n = app.search.matches.len();
    let text = format!(
        " search: {} | {} match(es) | Enter=next Up/Down=prev Esc=close ",
        app.search.query, n,
    );
    f.render_widget(
        Paragraph::new(text).style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        area,
    );
}

/// Render the input row as a rounded top+bottom border frame around the
/// wrapped text, growing with content. The prompt glyph leads the first
/// wrapped line (dim gray while the agent is working, white otherwise); the
/// border is dim gray, yellow while an approval is pending. Wrapped lines are
/// pre-computed by the editor at the same column count used for the height,
/// so the rendered rows and the reserved height never drift.
pub(super) fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let content_cols = (area.width as usize).saturating_sub(4);
    let border_color = if app.approval.is_some() || app.ask_question.is_some() {
        Color::Yellow
    } else {
        Color::Gray
    };
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let glyph_style = Style::new().fg(Color::Cyan);
    let body_style = Style::new().fg(Color::Reset);
    // The invert caret hides when the terminal window is unfocused (a
    // render-placeholder gates its invert cursor on terminalFocus).
    // set_cursor_position still parks the hidden cursor at the caret so IME
    // preedit lands correctly on refocus.
    let cursor_style = if app.terminal_focused {
        Style::new().bg(Color::White).fg(Color::Black)
    } else {
        body_style
    };
    let wrapped = app.input.wrapped_lines(content_cols);
    // Show the cursor even while a run is in flight: queueing, recalling,
    // and editing queued input are all busy-period flows, so hiding the
    // cursor then leaves the user blind to edits (and to a click that did
    // move the model cursor but rendered nowhere).
    let (crow, ccol) = app.input.cursor_position(content_cols);
    let cursor_byte = app.input.cursor();
    let lines: Vec<Line> = wrapped
        .iter()
        .enumerate()
        .map(|(i, wl)| {
            let prefix: Span<'static> = if i == 0 {
                Span::styled("❯ ", glyph_style)
            } else {
                Span::raw("  ")
            };
            let mut spans: Vec<Span<'static>> = vec![prefix];
            if i == crow {
                let cb = cursor_byte
                    .saturating_sub(wl.start_offset)
                    .min(wl.text.len());
                let before = &wl.text[..cb];
                let rest = &wl.text[cb..];
                if !before.is_empty() {
                    spans.push(Span::styled(before.to_string(), body_style));
                }
                if let Some(g) = rest.graphemes(true).next() {
                    spans.push(Span::styled(g.to_string(), cursor_style));
                    let after = &rest[g.len()..];
                    if !after.is_empty() {
                        spans.push(Span::styled(after.to_string(), body_style));
                    }
                } else {
                    // Cursor past the text end: an inverted space block.
                    spans.push(Span::styled(" ".to_string(), cursor_style));
                    // Empty input: a dim placeholder hint that vanishes on
                    // the first keystroke, so no welcome line floats above.
                    if wl.text.is_empty() && wl.start_offset == 0 {
                        spans.push(Span::styled(
                            "let's build, or / for commands".to_string(),
                            Style::new().fg(Color::DarkGray),
                        ));
                    }
                }
            } else {
                spans.push(Span::styled(wl.text.clone(), body_style));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
    // Park the terminal's physical cursor at the caret so the terminal
    // renders IME preedit text at the caret (terminals draw preedit at the
    // physical cursor position). The native cursor is hidden (Hide in
    // app.rs), so this positions the hidden cursor only — it does not bring
    // back a block cursor. Without this the cursor stays where ratatui left
    // it (the last drawn cell) and the preedit drifts away from the caret.
    // Prefix is 2 cols (the prompt glyph or its continuation indent). Clamp
    // the row so a long paste (crow past the visible input height) does not
    // park the cursor - and the IME preedit window - outside the input box.
    let crow = (crow as u16).min(inner.height.saturating_sub(1));
    f.set_cursor_position((
        inner.x.saturating_add(2).saturating_add(ccol as u16),
        inner.y.saturating_add(crow),
    ));
}
