//! @-triggered skill picker. Renders into the overlay slot shared with the
//! / palette (inline, pushes transcript up). Lists discovered skills with
//! Up/Down navigation, filter-as-you-type, and Enter to insert
//! @skill:name into the input box. Mirrors the slash-palette shape so
//! the two activation surfaces read as one interaction idiom.

pub(crate) mod state;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph},
};

use crate::state::App;
use crate::view::line_wrap::truncate_width;
use crate::view::skills_pane::display_order;
use houyicoder_protocol::frontend::skills::SkillEntry;
use unicode_width::UnicodeWidthStr;

/// The filtered skill list. The input text after @ is the filter query;
/// skills whose name matches (case-insensitive substring) are kept.
/// Shared by draw() and the key handlers so navigation and selection
/// operate on the same filtered list the user sees.
pub(crate) fn filtered_skills(app: &App) -> Vec<&SkillEntry> {
    let query = app
        .ui
        .input
        .editor
        .value()
        .strip_prefix('@')
        .unwrap_or_else(|| app.ui.input.editor.value());
    let ordered = display_order(&app.sessions.current.capabilities.skills.entries);
    if query.is_empty() {
        ordered
    } else {
        ordered
            .iter()
            .filter(|s| s.name.to_lowercase().contains(&query.to_lowercase()))
            .copied()
            .collect()
    }
}

/// Render the skill picker into the given area (the overlay slot shared
/// with the / palette). Inline — pushes the transcript up, does not
/// float over it. The filter query is read from the input text after
/// the @ trigger char.
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let filtered = filtered_skills(app);
    let count = filtered.len();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan))
        .title(format!(
            " @ skills \u{00b7} {count} found \u{00b7} Up/Down to select \u{00b7} Enter to insert \u{00b7} Esc to close "
        ));
    f.render_widget(block, area);

    let inner = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .inner(area);

    if filtered.is_empty() {
        f.render_widget(
            Paragraph::new("  no skills match").style(Style::new().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let items: Vec<ListItem> = filtered
        .iter()
        .map(|s| ListItem::new(format_one(s, inner.width)))
        .collect();
    let mut state = ListState::default();
    state.select(Some(
        app.ui
            .navigation
            .skill_picker
            .sel
            .get()
            .min(filtered.len().saturating_sub(1)),
    ));
    let list = List::new(items)
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    f.render_stateful_widget(list, inner, &mut state);
}

/// One row: the invocation gate, the skill name (bold), then a dim one-line
/// description truncated to fit the popover width (with an ellipsis when
/// cut), so the eye scans names and reads descriptions only when needed
/// without border overflow. The description budget is computed per-row from
/// the name width, so a long name clips the description (not the border).
fn format_one(s: &SkillEntry, inner_width: u16) -> Line<'static> {
    let usable = s.user_invocable || s.invocable;
    let glyph = if usable { "✓" } else { "✗" };
    let glyph_color = if usable {
        Color::Green
    } else {
        Color::DarkGray
    };
    let name_w = UnicodeWidthStr::width(s.name.as_str());
    // glyph + space + name + separator(2) + highlight symbol(2)
    let reserved = 1 + 1 + name_w + 2 + 2;
    let desc_max = (inner_width as usize).saturating_sub(reserved);
    Line::from(vec![
        Span::styled(glyph.to_string(), Style::new().fg(glyph_color)),
        Span::raw(" "),
        Span::styled(
            s.name.clone(),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            truncate_width(&s.description, desc_max),
            Style::new().fg(Color::DarkGray),
        ),
    ])
}

#[cfg(test)]
mod tests;
