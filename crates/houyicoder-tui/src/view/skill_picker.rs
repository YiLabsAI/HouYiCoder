//! @-triggered skill picker. Renders into the overlay slot shared with the
//! / palette (inline, pushes transcript up). Lists discovered skills with
//! Up/Down navigation, filter-as-you-type, and Enter to insert
//! @skill:name into the input box. Mirrors the slash-palette shape so
//! the two activation surfaces read as one interaction idiom.

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

/// The filtered skill list. The input text after @ is the filter query;
/// skills whose name matches (case-insensitive substring) are kept.
/// Shared by draw() and the key handlers so navigation and selection
/// operate on the same filtered list the user sees.
pub(crate) fn filtered_skills(
    app: &App,
) -> Vec<&houyicoder_protocol::frontend::skills::SkillEntry> {
    let query = app
        .input
        .value()
        .strip_prefix('@')
        .unwrap_or_else(|| app.input.value());
    let ordered = display_order(&app.skill_entries);
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
        app.skill_picker_sel
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
fn format_one(
    s: &houyicoder_protocol::frontend::skills::SkillEntry,
    inner_width: u16,
) -> Line<'static> {
    let glyph = if s.invocable { "✓" } else { "✗" };
    let glyph_color = if s.invocable {
        Color::Green
    } else {
        Color::DarkGray
    };
    let name_w = unicode_width::UnicodeWidthStr::width(s.name.as_str());
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
mod tests {
    use super::*;

    #[test]
    fn test_row_spans() {
        let entry = houyicoder_protocol::frontend::skills::SkillEntry {
            name: "ego-browser".to_string(),
            description: "browse the web".to_string(),
            origin: "project".to_string(),
            invocable: true,
            body_token_estimate: 120,
            usage: None,
        };
        let line = format_one(&entry, 40);
        assert!(line.spans.len() >= 4, "glyph + name + sep + desc");
    }

    /// A long name + long description must not overflow a narrow inner width:
    /// the description clips to 0 (or near-0), not the border.
    #[test]
    fn test_long_name_clips_desc() {
        let entry = houyicoder_protocol::frontend::skills::SkillEntry {
            name: "a-very-long-skill-name-that-eats-width".to_string(),
            description: "a description that would overflow".to_string(),
            origin: "user".to_string(),
            invocable: true,
            body_token_estimate: 100,
            usage: None,
        };
        let inner_w = 30u16;
        let line = format_one(&entry, inner_w);
        // The description span text width must fit within inner_w minus the
        // reserved prefix (glyph + space + name + separator + highlight).
        let name_w = unicode_width::UnicodeWidthStr::width(entry.name.as_str());
        let reserved = 1 + 1 + name_w + 2 + 2;
        let max_desc = (inner_w as usize).saturating_sub(reserved);
        let desc_span = &line.spans[4];
        let desc_w = unicode_width::UnicodeWidthStr::width(&*desc_span.content);
        assert!(
            desc_w <= max_desc,
            "desc width {desc_w} must fit budget {max_desc}"
        );
    }

    /// The picker renders the skill name, the gate glyph, and the nav hint
    /// when open on the working screen.
    #[test]
    fn test_picker_renders_open() {
        let mut app = crate::composition::app();
        app.screen = crate::state::Screen::Working;
        app.skill_entries = vec![houyicoder_protocol::frontend::skills::SkillEntry {
            name: "alpha".to_string(),
            description: "does alpha".to_string(),
            origin: "user".to_string(),
            invocable: true,
            body_token_estimate: 100,
            usage: None,
        }];
        app.skill_picker_open = true;
        let out = crate::test_support::render_text(&app, 80, 24);
        assert!(out.contains("alpha"), "skill name in picker: {out}");
        assert!(out.contains("Enter to insert"), "nav hint in picker: {out}");
    }

    /// The picker shows the empty-state line when no skills are discovered.
    #[test]
    fn test_picker_empty_state() {
        let mut app = crate::composition::app();
        app.screen = crate::state::Screen::Working;
        app.skill_picker_open = true;
        let out = crate::test_support::render_text(&app, 80, 24);
        assert!(
            out.contains("no skills match"),
            "empty state in picker: {out}"
        );
    }

    /// The picker filters skills by the text typed after @.
    #[test]
    fn test_picker_filter() {
        let mut app = crate::composition::app();
        app.screen = crate::state::Screen::Working;
        app.skill_entries = vec![
            houyicoder_protocol::frontend::skills::SkillEntry {
                name: "alpha".to_string(),
                description: "a".to_string(),
                origin: "user".to_string(),
                invocable: true,
                body_token_estimate: 100,
                usage: None,
            },
            houyicoder_protocol::frontend::skills::SkillEntry {
                name: "beta".to_string(),
                description: "b".to_string(),
                origin: "user".to_string(),
                invocable: true,
                body_token_estimate: 200,
                usage: None,
            },
        ];
        app.skill_picker_open = true;
        app.input.push('@');
        app.input.push('b');
        let out = crate::test_support::render_text(&app, 80, 24);
        assert!(out.contains("beta"), "filter matches beta: {out}");
        assert!(!out.contains("alpha"), "alpha filtered out: {out}");
    }

    /// The picker is not visible when closed (no title leaks onto the screen).
    #[test]
    fn test_picker_hidden_closed() {
        let mut app = crate::composition::app();
        app.screen = crate::state::Screen::Working;
        app.skill_entries = vec![houyicoder_protocol::frontend::skills::SkillEntry {
            name: "alpha".to_string(),
            description: "does alpha".to_string(),
            origin: "user".to_string(),
            invocable: true,
            body_token_estimate: 100,
            usage: None,
        }];
        let out = crate::test_support::render_text(&app, 80, 24);
        assert!(
            !out.contains("Enter to insert"),
            "picker title must not render when closed: {out}"
        );
    }
}
