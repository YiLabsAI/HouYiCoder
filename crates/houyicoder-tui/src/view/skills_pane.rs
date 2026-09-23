//! /skills pane content: an interactive list of discovered skills with a
//! detail drill-down. Grouped by discovery source; cursor selection via
//! Up/Down; Enter opens the detail; t toggles session-scoped disable.
//!
//! The detail pins the skill header and the usage line, and scrolls only
//! the body, so a long SKILL.md never pushes the invocation cost off screen.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use houyicoder_protocol::frontend::skills::SkillEntry;

use crate::skills_state::SkillDetail;
use crate::state::App;
use crate::view::line_wrap::max_scroll_offset;
use crate::view::navigation::key_hint;

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

/// Map a raw origin key (snake_case) to its display label. Returns the
/// key as-is if no matching group is found (defensive).
fn origin_label(origin: &str) -> &str {
    ORIGIN_ORDER
        .iter()
        .find(|g| g.key == origin)
        .map(|g| g.label)
        .unwrap_or(origin)
}

/// The label and canonical scan path for one origin key, when it is known.
fn origin_group(origin: &str) -> Option<&'static OriginGroup> {
    ORIGIN_ORDER.iter().find(|g| g.key == origin)
}

const ORIGIN_ORDER: &[OriginGroup] = &[
    OriginGroup {
        key: "managed",
        label: "Managed",
        path: "/etc/houyicoder/skills/",
    },
    OriginGroup {
        key: "user",
        label: "Native",
        path: "~/.houyicoder/skills/",
    },
    OriginGroup {
        key: "project",
        label: "Native",
        path: ".houyicoder/skills/",
    },
    OriginGroup {
        key: "claude_eco",
        label: "Ecosystem",
        path: ".claude/skills/",
    },
    OriginGroup {
        key: "agents",
        label: "Spec",
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
/// name within each group. Both render and key-dispatch resolve the list cursor
/// through this so the highlighted row and the acted-on entry match.
pub(crate) fn display_order(entries: &[SkillEntry]) -> Vec<&SkillEntry> {
    let mut out: Vec<&SkillEntry> = Vec::new();
    for group in ORIGIN_ORDER {
        let mut members: Vec<&SkillEntry> =
            entries.iter().filter(|e| e.origin == group.key).collect();
        members.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(members);
    }
    out
}

pub(crate) fn draw_content(f: &mut Frame, inner: Rect, app: &App) {
    if let Some(detail) = app.skills_pane.detail() {
        draw_detail(f, inner, app, detail);
        return;
    }
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
        let sel = app
            .skills_pane
            .cursor()
            .min(ordered.len().saturating_sub(1));
        f.render_widget(
            Paragraph::new(grouped_lines(&ordered, sel, &app.skill_disabled)),
            chunks[2],
        );
    }

    f.render_widget(
        Paragraph::new(key_hint(&[
            ("Up/Down", "select"),
            ("Enter", "open"),
            ("Esc", "close"),
        ]))
        .style(Style::new().fg(Color::DarkGray)),
        chunks[3],
    );
}

/// The detail: a pinned two-row header (identity + origin), the scrollable
/// body, a pinned usage line, and the key hints. Only the body region
/// scrolls, so the cost and the usage stay visible however long the body is.
fn draw_detail(f: &mut Frame, inner: Rect, app: &App, detail: &SkillDetail) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let name = match detail {
        SkillDetail::Loading { name, .. } | SkillDetail::Open { name, .. } => name.as_str(),
    };
    let entry = app.skill_entries.iter().find(|e| e.name == name);
    let user_disabled = app.skill_disabled.contains(name);
    f.render_widget(
        Paragraph::new(header_lines(entry, name, user_disabled)),
        rows[0],
    );
    f.render_widget(Paragraph::new(usage_line(entry)), rows[2]);

    let mut body: Vec<Line<'static>> = Vec::new();
    if let Some(entry) = entry {
        body.push(Line::from(entry.description.clone()).style(Style::new().fg(Color::DarkGray)));
        body.push(Line::from(""));
    }
    let offset = match detail {
        SkillDetail::Loading { .. } => {
            body.push(Line::from("loading body...").style(Style::new().fg(Color::DarkGray)));
            0
        }
        SkillDetail::Open {
            body: text, offset, ..
        } => {
            match text {
                Some(text) => body.extend(text.lines().map(|line| Line::from(line.to_string()))),
                None => {
                    body.push(Line::from("body unavailable").style(Style::new().fg(Color::Red)))
                }
            }
            offset.get()
        }
    };
    let max_offset = max_scroll_offset(&body, rows[1].width, rows[1].height);
    app.skills_pane.set_detail_max_offset(max_offset);
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((offset.min(max_offset), 0)),
        rows[1],
    );
    f.render_widget(
        Paragraph::new(key_hint(&[
            ("Up/Down", "scroll"),
            ("t", "toggle"),
            ("Esc", "back"),
        ]))
        .style(Style::new().fg(Color::DarkGray)),
        rows[3],
    );
}

/// The pinned identity row plus the origin row (label and scan path), the
/// same pair the list shows as a group header. The gate glyph reflects the
/// session disable toggle, so the detail and the row it was opened from
/// never disagree about whether the skill is usable.
fn header_lines(entry: Option<&SkillEntry>, name: &str, user_disabled: bool) -> Vec<Line<'static>> {
    let mut head = vec![Span::styled(
        name.to_string(),
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )];
    let mut origin = Vec::new();
    if let Some(entry) = entry {
        let (glyph, color) = gate_glyph(entry, user_disabled);
        head.push(Span::raw("  "));
        head.push(Span::styled(glyph, Style::new().fg(color)));
        head.push(Span::styled(
            format!("  ~{} tok", entry.body_token_estimate),
            Style::new().fg(Color::DarkGray),
        ));
        match origin_group(&entry.origin) {
            Some(group) => {
                origin.push(Span::styled(
                    group.label.to_string(),
                    Style::new().fg(Color::Cyan),
                ));
                origin.push(Span::raw(" — "));
                origin.push(Span::styled(
                    group.path.to_string(),
                    Style::new().fg(Color::DarkGray),
                ));
            }
            None => origin.push(Span::styled(
                format!("origin: {}", origin_label(&entry.origin)),
                Style::new().fg(Color::DarkGray),
            )),
        }
    }
    vec![Line::from(head), Line::from(origin)]
}

/// The invocation gate glyph and colour: disabled by the user, invocable by
/// the model or the user, or blocked by frontmatter.
fn gate_glyph(entry: &SkillEntry, user_disabled: bool) -> (&'static str, Color) {
    if user_disabled {
        ("○", Color::DarkGray)
    } else if entry.user_invocable || entry.invocable {
        ("✓", Color::Green)
    } else {
        ("✗", Color::Red)
    }
}

/// The usage line: session-scoped invocation stats. Shows invocations,
/// refusals, last-used relative time, and a token estimate
/// (body_token_estimate × invocations). Labeled "this session" so the
/// user knows the count is not all-time. Omitted when no usage data
/// (registry does not track) or never invoked.
fn usage_line(entry: Option<&SkillEntry>) -> Line<'static> {
    let Some(usage) = entry.and_then(|e| e.usage.as_ref()) else {
        return Line::from("");
    };
    if usage.invocations == 0 && usage.refusals == 0 {
        return Line::from(Span::styled(
            "never invoked this session".to_string(),
            Style::new().fg(Color::DarkGray),
        ));
    }
    let now = crate::view::relative_time::now_epoch_secs();
    let last = crate::view::relative_time::relative_time(now, usage.last_used_secs);
    let mut parts = format!(
        "invoked {}× · {} refused · last {} (this session)",
        usage.invocations, usage.refusals, last
    );
    if usage.invocations > 0
        && let Some(tokens) = entry.map(|e| e.body_token_estimate).filter(|t| *t > 0)
    {
        parts.push_str(&format!(
            " · ~{} tok est.",
            usage.invocations * tokens as u64
        ));
    }
    Line::from(Span::styled(parts, Style::new().fg(Color::DarkGray)))
}

/// Build the grouped render: a header line per origin (label + canonical
/// path), then each skill in that group on its own line carrying the
/// model-invocation gate and the body token estimate. One line per skill
/// keeps the list scannable; the gate + token sit at the row tail and only
/// clip for very long descriptions (the name always stays visible).
fn grouped_lines(
    ordered: &[&SkillEntry],
    cursor: usize,
    disabled: &std::collections::HashSet<String>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let mut prev_origin: &str = "";
    for (idx, s) in ordered.iter().enumerate() {
        if s.origin != prev_origin {
            if let Some(group) = origin_group(&s.origin) {
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
        let (glyph, color) = gate_glyph(s, disabled.contains(&s.name));
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
