//! The artifact pane: a referenced document opened in-TUI for inline review
//! and modal editing, and the closed review loop (edit -> proposed change ->
//! approve/reject -> applied). Content on the left (focus-centered window so
//! the real document stays usable without a scroll subsystem), the review
//! surface on the right. Writing, review, and editing happen on one surface,
//! with no external editor.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
};

use crate::state::App;
use crate::view::{components, status};

/// Render the artifact pane: header (path + line cursor + pending flag),
/// content left, review surface right.
pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let total = app.artifact.current_lines().len();
    let idx = app.artifact.focus().min(total.saturating_sub(1));
    let pending = app.artifact.pending_proposal().is_some();
    let base = format!(
        "artifact | {} | line {}/{}{}",
        app.artifact.path(),
        idx + 1,
        total,
        if pending { " | pending review" } else { "" },
    );
    let title = if status::is_focus(app) {
        format!(" {} | {} ", status::progress_str(app), base)
    } else {
        format!(" {base} ")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Side-by-side reads cramped below ~100 cols: stack content above the
    // review surface so content keeps the full inner width.
    if inner.width < 100 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(inner);
        draw_content(f, rows[0], app);
        draw_review(f, rows[1], app);
    } else {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(inner);
        draw_content(f, cols[0], app);
        draw_review(f, cols[1], app);
    }
}

/// Left column: the document content, line-numbered, in a focus-centered window
/// so the focused line stays visible on a long real document without a scroll
/// subsystem. The focused line is Cyan + BOLD with a marker; lines carrying an
/// annotation get a star.
fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" content (structured) ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let all = app.artifact.current_lines();
    let marks = app.artifact.applied_marks();
    let total = all.len();
    let focus = app.artifact.focus().min(total.saturating_sub(1));
    let height = inner.height as usize;
    let start = focus.saturating_sub(height / 2);
    let end = (start + height).min(total);
    let white = Style::new().fg(Color::White);
    let dim = Style::new().fg(Color::DarkGray);
    let accent = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let green = Style::new().fg(Color::Green);
    let lines: Vec<Line> = (start..end)
        .map(|i| {
            let focused = i == focus;
            let noted = app.artifact.has_annotation(i);
            let applied = marks.get(i).copied().unwrap_or(false);
            let num = Span::styled(format!("{:>3} ", i + 1), dim);
            let marker = Span::styled(
                if focused { "> " } else { "  " },
                if focused { accent } else { dim },
            );
            let note = Span::styled(
                if noted { "*" } else { " " },
                if noted { accent } else { dim },
            );
            let ok = Span::styled(if applied { "ok" } else { "  " }, green);
            let body = Span::styled(
                all.get(i).cloned().unwrap_or_default(),
                if focused { accent } else { white },
            );
            Line::from(vec![num, marker, note, ok, Span::raw(" "), body])
        })
        .collect();
    // No Wrap: a long line truncates at the column edge instead of wrapping,
    // so the line-number column stays aligned (wrapping drifted the number onto
    // the continuation row).
    f.render_widget(Paragraph::new(lines), inner);
}

/// Right column: the review surface. When a proposed edit is pending it shows
/// the diff (rationale + before/after) and the approve/reject row; otherwise it
/// shows the focused line's annotations and the key hints.
fn draw_review(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" review (inline) ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let white = Style::new().fg(Color::White);
    let dim = Style::new().fg(Color::DarkGray);
    let accent = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = Vec::new();
    if let Some(proposal) = app.artifact.pending_proposal() {
        lines.push(Line::from(" proposed edit (pending)").style(accent));
        lines.push(Line::from(format!("   rationale: {}", proposal.rationale)).style(white));
        lines
            .push(Line::from(format!("   before: {}", proposal.original.join(" | "))).style(white));
        lines
            .push(Line::from(format!("   after:  {}", proposal.proposed.join(" | "))).style(white));
        lines.push(Line::from(""));
        lines.push(components::approve_reject_row(proposal.verdict));
        lines.push(Line::from("   a = apply    r = reject").style(dim));
    } else {
        let focus = app.artifact.focus();
        lines.push(Line::from(format!(" line {} ", focus + 1)).style(accent));
        let notes = app.artifact.annotations_for(focus);
        if notes.is_empty() {
            lines.push(Line::from("   (no annotations yet)").style(dim));
        } else {
            for (i, note) in notes.iter().enumerate() {
                lines.push(Line::from(format!("   {}. {}", i + 1, note.text)).style(white));
            }
        }
        lines.push(Line::from(""));
        lines.push(
            Line::from(format!(
                " {} annotations, {} applied",
                app.artifact.annotation_count(),
                app.artifact.applied_count(),
            ))
            .style(dim),
        );
        if let Some(last) = app.artifact.applied_last() {
            lines.push(
                Line::from(format!(
                    " last applied: {} (line {})",
                    last.rationale,
                    last.line_start + 1,
                ))
                .style(dim),
            );
        }
        lines.push(Line::from(" c=replace o=insert d=delete i=nl").style(dim));
        lines.push(Line::from(" Up/Down = line  PgUp/Dn = page").style(dim));
        lines.push(Line::from(" /artifact-save [path] = write to disk").style(dim));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
