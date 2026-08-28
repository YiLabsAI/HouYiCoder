//! /skills pane content: a read-only list of discovered skills, grouped by
//! their discovery source so the user sees where each skill came from
//! (managed policy, user, project, ecosystem compat, ...). Each group header
//! carries the canonical scan path so the user knows where to drop a new
//! skill to land in that group.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::state::App;

pub(crate) const SKILLS_PANE_HEIGHT: u16 = 16;

/// Max chars of a skill description shown before truncation. Leaves room
/// for the prefix and the tail (gate + token cost) so the cost stays
/// visible at 80 cols even for long descriptions.
const DESC_BUDGET: usize = 40;

/// Truncate a description to budget chars, appending an ellipsis when it
/// is longer. Char-count (not byte-count) so multi-byte runes do not split.
fn truncate_desc(desc: &str, budget: usize) -> String {
    if desc.chars().count() <= budget {
        return desc.to_string();
    }
    let mut out: String = desc.chars().take(budget.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// A display row: label + canonical scan path for a discovery origin. Only
/// origins that appear in the snapshot render, in fixed precedence order
/// (managed first, local last), so the group order is stable across snapshots.
struct OriginGroup {
    key: &'static str,
    label: &'static str,
    path: &'static str,
}

const ORIGIN_ORDER: &[OriginGroup] = &[
    OriginGroup {
        key: "managed",
        label: "Managed",
        path: "/etc/houyicoder/skills/",
    },
    OriginGroup {
        key: "user",
        label: "User",
        path: "~/.houyicoder/skills/",
    },
    OriginGroup {
        key: "project",
        label: "Project",
        path: ".houyicoder/skills/",
    },
    OriginGroup {
        key: "claude_eco",
        label: "Claude eco",
        path: ".claude/skills/",
    },
    OriginGroup {
        key: "agents",
        label: "Agents",
        path: ".agents/skills/",
    },
    OriginGroup {
        key: "mcp",
        label: "MCP",
        path: "(mcp server)",
    },
    OriginGroup {
        key: "local",
        label: "Local",
        path: "(local override)",
    },
];

pub(crate) fn draw_content(f: &mut Frame, inner: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Skills",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )])),
        chunks[0],
    );

    let count = app.skill_entries.len();
    f.render_widget(
        Paragraph::new(format!("{count} skills discovered"))
            .style(Style::new().fg(Color::DarkGray)),
        chunks[1],
    );

    if app.skill_entries.is_empty() {
        f.render_widget(
            Paragraph::new("No skills found. Create skills in .houyicoder/skills/")
                .style(Style::new().fg(Color::DarkGray)),
            chunks[2],
        );
    } else {
        f.render_widget(Paragraph::new(grouped_lines(&app.skill_entries)), chunks[2]);
    }

    f.render_widget(
        Paragraph::new("Esc to close").style(Style::new().fg(Color::DarkGray)),
        chunks[3],
    );
}

/// Build the grouped render: a header line per origin (label + canonical
/// path), then each skill in that group on its own line carrying the
/// model-invocation gate and the body token estimate. One line per skill
/// keeps the list scannable; the gate + token sit at the row tail and only
/// clip for very long descriptions (the name always stays visible).
fn grouped_lines(
    entries: &[houyicoder_protocol::frontend::skills::SkillEntry],
) -> Vec<Line<'static>> {
    use houyicoder_protocol::frontend::skills::SkillEntry;
    let mut lines: Vec<Line> = Vec::new();
    for group in ORIGIN_ORDER {
        let mut members: Vec<&SkillEntry> =
            entries.iter().filter(|e| e.origin == group.key).collect();
        if members.is_empty() {
            continue;
        }
        // Stable sort by name within a group so the order does not flip on
        // re-scan (precedence across groups is fixed by ORIGIN_ORDER).
        members.sort_by(|a, b| a.name.cmp(&b.name));
        lines.push(Line::from(vec![
            Span::styled(group.label.to_string(), Style::new().fg(Color::Cyan)),
            Span::raw(" — "),
            Span::styled(group.path.to_string(), Style::new().fg(Color::DarkGray)),
        ]));
        for s in members {
            // One line per skill so the list stays scannable for a real
            // skill library. The description is pre-truncated (with an
            // ellipsis) so the invocation gate + token cost at the tail
            // never clip off the right edge for long descriptions — the
            // cost is the commit-before-invoking signal, it must stay
            // visible.
            let desc = truncate_desc(&s.description, DESC_BUDGET);
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("- {}: ", s.name), Style::new().fg(Color::Yellow)),
                Span::raw(desc),
                Span::raw("  "),
                Span::styled(
                    if s.invocable { "✓" } else { "✗" },
                    Style::new().fg(if s.invocable {
                        Color::Green
                    } else {
                        Color::Red
                    }),
                ),
                Span::styled(
                    format!(" ~{} tok", s.body_token_estimate),
                    Style::new().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    lines
}
