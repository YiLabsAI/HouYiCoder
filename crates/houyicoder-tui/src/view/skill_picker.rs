//! @-triggered skill picker popover. Bottom-anchored over the transcript
//! (drops down from the input area), small, lists discovered skills with
//! Up/Down navigation and Enter to insert @skill:name into the input and
//! submit. Mirrors the slash-palette popover shape so the two activation
//! surfaces (commands via /, skills via @) read as one interaction idiom.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use crate::state::App;
use crate::view::line_wrap::truncate_width;
use crate::view::skills_pane::display_order;

/// Maximum rows the popover shows before scrolling. Keeps the popover small so
/// it never covers the whole working surface.
const MAX_VISIBLE: usize = 8;

/// Maximum popover width in columns. A small bottom-anchored box, never
/// full-screen. Long descriptions truncate to fit this width.
const MAX_WIDTH: u16 = 64;

/// Render the skill picker as a bottom-anchored popover in the given area.
/// The area should sit just above the input box (see area()).
pub fn draw(f: &mut Frame, app: &App, screen: Rect) {
    let area = area(screen);
    f.render_widget(Clear, area);
    let ordered = display_order(&app.skill_entries);
    let count = ordered.len();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan))
        .title(format!(
            " @ skills | {count} found | Up/Down to select \u{00b7} Enter to insert \u{00b7} Esc to close "
        ));
    f.render_widget(block, area);

    let inner = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .inner(area);

    if ordered.is_empty() {
        f.render_widget(
            Paragraph::new("  no skills discovered").style(Style::new().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let items: Vec<ListItem> = ordered
        .iter()
        .map(|s| ListItem::new(format_one(s, inner.width)))
        .collect();
    let mut state = ListState::default();
    state.select(Some(
        app.skill_picker_sel
            .get()
            .min(ordered.len().saturating_sub(1)),
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

/// The bottom-anchored popover rect: a small box that drops down from just
/// above the input row, never full-screen. Width is capped at MAX_WIDTH
/// (centered) so the popover reads as a small overlay; height is capped so
/// at most MAX_VISIBLE rows + border fit.
pub fn area(screen: Rect) -> Rect {
    let visible = MAX_VISIBLE.min(screen.height as usize / 2);
    let height = (visible + 2) as u16;
    let bottom_gap = 4;
    let top = screen.height.saturating_sub(height + bottom_gap);
    let width = screen.width.min(MAX_WIDTH);
    let x = screen.x + (screen.width.saturating_sub(width)) / 2;
    Rect::new(x, screen.y + top, width, height.min(screen.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_area_small_centered() {
        let screen = Rect::new(0, 0, 80, 24);
        let a = area(screen);
        assert!(a.height <= 12, "popover must stay small, got {}", a.height);
        assert!(
            a.y + a.height <= screen.y + screen.height,
            "popover must not overflow the screen"
        );
        assert!(
            a.width <= MAX_WIDTH,
            "popover width must be capped, got {}",
            a.width
        );
        assert_eq!(
            a.x - screen.x,
            (screen.width - a.width) / 2,
            "popover should be horizontally centered"
        );
    }

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
            out.contains("no skills discovered"),
            "empty state in picker: {out}"
        );
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
