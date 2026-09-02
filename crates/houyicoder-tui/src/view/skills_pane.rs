//! /skills pane content: an interactive list of discovered skills with a
//! detail drill-down. Grouped by discovery source; cursor selection via
//! Up/Down; Enter opens the detail; t toggles session-scoped disable.

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

/// The display order: entries grouped by origin (ORIGIN_ORDER), sorted by
/// name within each group. Both render and key-dispatch resolve skill_sel
/// through this so the highlighted row and the acted-on entry match.
pub(crate) fn display_order(
    entries: &[houyicoder_protocol::frontend::skills::SkillEntry],
) -> Vec<&houyicoder_protocol::frontend::skills::SkillEntry> {
    let mut out: Vec<&houyicoder_protocol::frontend::skills::SkillEntry> = Vec::new();
    for group in ORIGIN_ORDER {
        let mut members: Vec<&houyicoder_protocol::frontend::skills::SkillEntry> =
            entries.iter().filter(|e| e.origin == group.key).collect();
        members.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(members);
    }
    out
}

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
        let ordered = display_order(&app.skill_entries);
        let sel = app.skill_sel.get().min(ordered.len().saturating_sub(1));
        if app.skill_level.get() == 1 {
            if let Some(entry) = ordered.get(sel) {
                let disabled = app.skill_disabled.contains(&entry.name);
                let lines = detail_lines(entry, disabled);
                f.render_widget(Paragraph::new(lines), chunks[2]);
            }
        } else {
            f.render_widget(
                Paragraph::new(grouped_lines(&ordered, sel, &app.skill_disabled)),
                chunks[2],
            );
        }
    }

    let footer = if app.skill_level.get() == 1 {
        "t toggle · Esc back"
    } else {
        "Up/Down select · enter open · Esc close"
    };
    f.render_widget(
        Paragraph::new(footer).style(Style::new().fg(Color::DarkGray)),
        chunks[3],
    );
}

/// Build the grouped render: a header line per origin (label + canonical
/// path), then each skill in that group on its own line carrying the
/// model-invocation gate and the body token estimate. One line per skill
/// keeps the list scannable; the gate + token sit at the row tail and only
/// clip for very long descriptions (the name always stays visible).
fn grouped_lines(
    ordered: &[&houyicoder_protocol::frontend::skills::SkillEntry],
    cursor: usize,
    disabled: &std::collections::HashSet<String>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let mut prev_origin: &str = "";
    for (idx, s) in ordered.iter().enumerate() {
        if s.origin != prev_origin {
            if let Some(group) = ORIGIN_ORDER.iter().find(|g| g.key == s.origin) {
                lines.push(Line::from(vec![
                    Span::styled(group.label.to_string(), Style::new().fg(Color::Cyan)),
                    Span::raw(" — "),
                    Span::styled(group.path.to_string(), Style::new().fg(Color::DarkGray)),
                ]));
            }
            prev_origin = &s.origin;
        }
        let desc = truncate_desc(&s.description, DESC_BUDGET);
        let is_selected = idx == cursor;
        let prefix = if is_selected { "▶ " } else { "  " };
        let user_disabled = disabled.contains(&s.name);
        let (glyph, color) = if user_disabled {
            ("○", Color::DarkGray)
        } else if s.invocable {
            ("✓", Color::Green)
        } else {
            ("✗", Color::Red)
        };
        lines.push(Line::from(vec![
            Span::raw(prefix),
            Span::styled(format!("- {}: ", s.name), Style::new().fg(Color::Yellow)),
            Span::raw(desc),
            Span::raw("  "),
            Span::styled(glyph, Style::new().fg(color)),
            Span::styled(
                format!(" ~{} tok", s.body_token_estimate),
                Style::new().fg(Color::DarkGray),
            ),
        ]));
    }
    lines
}

fn detail_lines(
    entry: &houyicoder_protocol::frontend::skills::SkillEntry,
    disabled: bool,
) -> Vec<Line<'static>> {
    let glyph = if disabled {
        "○ disabled"
    } else if entry.invocable {
        "✓ invocable"
    } else {
        "✗ frontmatter-disabled"
    };
    let color = if disabled {
        Color::DarkGray
    } else {
        Color::Green
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                entry.name.clone(),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(glyph, Style::new().fg(color)),
            Span::styled(
                format!("  ~{} tok", entry.body_token_estimate),
                Style::new().fg(Color::DarkGray),
            ),
        ]),
        Line::from(Span::styled(
            entry.description.clone(),
            Style::new().fg(Color::White),
        )),
        Line::from(Span::styled(
            format!("origin: {}", entry.origin),
            Style::new().fg(Color::DarkGray),
        )),
    ];
    // Usage line: session-scoped invocation stats. Shows invocations,
    // refusals, last-used relative time, and a token estimate
    // (body_token_estimate × invocations). Labeled "this session" so the
    // user knows the count is not all-time. Omitted when no usage data
    // (registry does not track) or never invoked.
    if let Some(usage) = &entry.usage {
        if usage.invocations == 0 && usage.refusals == 0 {
            lines.push(Line::from(Span::styled(
                "never invoked this session".to_string(),
                Style::new().fg(Color::DarkGray),
            )));
        } else {
            let now = crate::view::relative_time::now_epoch_secs();
            let last = crate::view::relative_time::relative_time(now, usage.last_used_secs);
            let mut parts = format!(
                "invoked {}× · {} refused · last {} (this session)",
                usage.invocations, usage.refusals, last
            );
            if usage.invocations > 0 && entry.body_token_estimate > 0 {
                let total = usage.invocations * entry.body_token_estimate as u64;
                parts.push_str(&format!(" · ~{} tok est.", total));
            }
            lines.push(Line::from(Span::styled(
                parts,
                Style::new().fg(Color::DarkGray),
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        if disabled {
            "t: enable   Esc: back"
        } else {
            "t: disable  Esc: back"
        },
        Style::new().fg(Color::DarkGray),
    )));
    lines
}
