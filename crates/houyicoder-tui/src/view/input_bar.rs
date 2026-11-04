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

use crate::input::{InputMode, WrappedLine};
use crate::selection::TAG_PLAIN;
use crate::state::App;

/// Render the inline Ctrl+F search bar (one line below the input box).
pub(super) fn draw_inline_search(f: &mut Frame, area: Rect, app: &App) {
    let n = app.ui.transcript_view.search_matches_len();
    let text = format!(
        " search: {} | {} match(es) | Enter=next Up/Down=prev Esc=close ",
        app.ui.transcript_view.search_query(),
        n,
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
    let shell_mode = app.ui.input.mode == InputMode::DirectShell;
    let border_color = if app.approval().is_some() || app.ask_question().is_some() {
        Color::Yellow
    } else if shell_mode {
        Color::Cyan
    } else {
        Color::Gray
    };
    let mut block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border_color));
    if shell_mode {
        // The title reports the sandbox label the session was composed with;
        // it cannot prove the fence. A label whose first word is off means the
        // session runs unfenced, so the warning color marks it; any other
        // label is shown as written rather than relabeled sandboxed, which
        // would claim a boundary the terminal has not measured.
        let label = app.sessions.current.sandbox.split_whitespace().next();
        let (title, title_color) = if label == Some("off") {
            (" bash · unfenced ".to_string(), Color::Yellow)
        } else {
            (
                format!(" bash · {} ", app.sessions.current.sandbox),
                Color::Cyan,
            )
        };
        block = block.title(Span::styled(
            title,
            Style::new().fg(title_color).add_modifier(Modifier::BOLD),
        ));
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    let glyph_style = if shell_mode {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Cyan)
    };
    let body_style = Style::new().fg(Color::Reset);
    let ghost_style = Style::new().fg(Color::DarkGray);
    // The painted caret hides when the terminal window is unfocused, and while
    // the child strip holds the keyboard: two carets on screen would claim the
    // keys reach both.
    let cursor_style = if app.ui.terminal_focused && !app.ui.fleet.is_focused() {
        Style::new().bg(Color::White).fg(Color::Black)
    } else {
        body_style
    };
    let wrapped = app.ui.input.editor.wrapped_lines(content_cols);
    publish_text_area(app, inner, &wrapped);
    // Show the cursor even while a run is in flight: queueing, recalling,
    // and editing queued input are all busy-period flows, so hiding the
    // cursor then leaves the user blind to edits (and to a click that did
    // move the model cursor but rendered nowhere).
    let (crow, ccol) = app.ui.input.editor.cursor_position(content_cols);
    let cursor_byte = app.ui.input.editor.cursor();
    let ghost = app.ui.input.ghost.as_deref();
    let lines: Vec<Line> = wrapped
        .iter()
        .enumerate()
        .map(|(i, wl)| {
            let prefix: Span<'static> = if i == 0 {
                if shell_mode {
                    Span::styled("$ ", glyph_style)
                } else {
                    Span::styled("❯ ", glyph_style)
                }
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
                    // Cursor past the text end: an inverted space block, then
                    // the ghost suffix in DirectShell mode, or the placeholder
                    // hint in Chat mode on an empty first line.
                    spans.push(Span::styled(" ".to_string(), cursor_style));
                    if let Some(suffix) = ghost {
                        spans.push(Span::styled(suffix.to_string(), ghost_style));
                    } else if wl.text.is_empty() && wl.start_offset == 0 && !shell_mode {
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
    let row = (crow as u16).min(inner.height.saturating_sub(1));
    app.ui.frame.feedback.native_cursor_position.set(Some((
        inner.x.saturating_add(2).saturating_add(ccol as u16),
        inner.y.saturating_add(row),
    )));
}

/// Publish the composer's text area and its wrapped rows for the selection and
/// click paths.
///
/// The text starts past the border and the prompt glyph, so the published rect
/// is the area a drag can select and a click can map, and the rows are the
/// draft without the glyph that leads it.
fn publish_text_area(app: &App, inner: Rect, wrapped: &[WrappedLine]) {
    app.ui.frame.layout.input_rect.set(Rect {
        x: inner.x.saturating_add(2),
        y: inner.y,
        width: inner.width.saturating_sub(2),
        height: inner.height,
    });
    let mut rows = app.ui.frame.layout.last_input_rows.borrow_mut();
    rows.clear();
    rows.extend(wrapped.iter().map(|wl| (TAG_PLAIN, wl.text.clone())));
}

#[cfg(test)]
mod tests;
