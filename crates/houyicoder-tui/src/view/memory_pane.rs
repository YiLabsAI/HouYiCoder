//! The /memory pane body content. A peer of search.rs draw_content and
//! capability draw_permission_content: the slash-command pane content
//! closures reused by the working-surface draw_command_pane template.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::state::App;
use crate::state::MemoryEntry;
use crate::state::enums::MemoryScopeTab;

/// The /memory pane body: a header line with the stored count, one row per
/// stored memory (key + one-line summary), the two toggle rows, and a footer
/// hint naming the show + toggle sub-commands. Pure render off app state. One
/// Paragraph of lines so the layout matches the /search pane (predictable for
/// render-cache tests); a long list wraps inside the pane area.
pub(super) fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    // Scope-tab header: the active tab is highlighted; the rest dim.
    // Left/Right cycles. All shows every root merged; the others narrow to
    // one physical scope (the "see this project's memories" filter).
    let tab = app.memory_scope_tab;
    let tabs = [
        (MemoryScopeTab::All, "All"),
        (MemoryScopeTab::User, "User"),
        (MemoryScopeTab::Project, "Project"),
        (MemoryScopeTab::Auto, "Auto"),
    ];
    let mut tab_spans: Vec<Span> = vec![Span::raw("  ")];
    for (i, (variant, label)) in tabs.iter().enumerate() {
        if i > 0 {
            tab_spans.push(Span::raw("  "));
        }
        if *variant == tab {
            tab_spans.push(Span::styled(
                format!("[{label}]"),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
        } else {
            tab_spans.push(Span::styled(*label, Style::new().fg(Color::DarkGray)));
        }
    }
    lines.push(Line::from(tab_spans));
    // Filter the list by the active scope + the text search (composed). Shared
    // with the d/enter actions so the cursor the user sees is the one the
    // action hits.
    let filtered =
        crate::command::render::filtered_memory(&app.memory_entries, tab, &app.memory_search);
    // When a text filter is set, show it as a row so the user sees the active
    // query (Esc clears it).
    if !app.memory_search.is_empty() {
        lines.push(
            Line::from(format!("  search: [{}]  (Esc clears)", app.memory_search))
                .style(Style::new().fg(Color::DarkGray)),
        );
    }
    let n = filtered.len();
    let cursor = app.memory_cursor.min(n.saturating_sub(1));
    lines.push(
        Line::from(format!("memory — {n} stored"))
            .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    );
    // Total scope distribution (across all entries, not the filtered view) so
    // the user sees where the memories live at a glance, even when narrowed.
    if !app.memory_entries.is_empty() {
        lines.push(
            Line::from(scope_distribution(&app.memory_entries))
                .style(Style::new().fg(Color::DarkGray)),
        );
    }
    if filtered.is_empty() {
        lines.push(Line::from("  (no memories yet)").style(Style::new().fg(Color::DarkGray)));
    } else {
        for (i, m) in filtered.iter().enumerate() {
            // Tag is scope dot source (e.g. project dot user) — the physical
            // storage root + the provenance category, the two orthogonal
            // dimensions. Scope drives the pane filter; source is the tag.
            let tag = format!("{}·{}", m.scope, m.source);
            let prefix = if i == cursor { "❯ " } else { "  " };
            let key_style = if i == cursor {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            };
            let mut spans: Vec<Span> = vec![
                Span::styled(prefix, Style::new().fg(Color::Cyan)),
                Span::styled(format!("[{tag}] "), Style::new().fg(Color::DarkGray)),
                Span::styled(m.topic.clone(), key_style),
            ];
            if !m.summary.is_empty() {
                spans.push(Span::styled(
                    format!(" — {}", m.summary),
                    Style::new().fg(Color::White),
                ));
            }
            lines.push(Line::from(spans));
        }
    }
    let am = if app.memory_toggles.auto_memory {
        "on"
    } else {
        "off"
    };
    let ad = if app.memory_toggles.auto_dream {
        "on"
    } else {
        "off"
    };
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  Auto-memory: ", Style::new().fg(Color::DarkGray)),
        Span::styled(am, Style::new().fg(Color::White)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Auto-dream: ", Style::new().fg(Color::DarkGray)),
        Span::styled(ad, Style::new().fg(Color::White)),
    ]));
    lines.push(Line::from(""));
    lines.push(
        Line::from(
            "  /memory <key> show · /memory toggle auto|dream · Left/Right scope · Esc close",
        )
        .style(Style::new().fg(Color::DarkGray)),
    );
    f.render_widget(Paragraph::new(lines), area);
}

/// One-line breakdown of how many memories live in each storage scope, across
/// the full entry list (not the filtered view). Shown dim under the header so
/// the user sees the distribution at a glance even when narrowed to one scope.
fn scope_distribution(entries: &[MemoryEntry]) -> String {
    let (mut u, mut p, mut a) = (0usize, 0usize, 0usize);
    for m in entries {
        match m.scope.as_str() {
            "user" => u += 1,
            "project" => p += 1,
            "auto" => a += 1,
            _ => {}
        }
    }
    format!("  {u} user / {p} project / {a} auto")
}
