//! Queue overlay render (Ctrl+G). Extracted from working.rs to keep it under
//! the file-size gate. See keys::handle_working for the dispatch + overlay_keys
//! for the key handler.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use crate::state::App;

/// Truncate to a char budget with ellipsis when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let mut t: String = chars[..max - 1].iter().collect();
    t.push('\u{2026}');
    t
}

/// Full queue overlay (Ctrl+G). Covers the transcript: every pending item as
/// a numbered row with a cursor, plus the action footer. e recalls, d deletes,
/// a recalls all. The per-item replacement for the old all-or-nothing pop.
pub fn draw_queue_overlay(f: &mut Frame, area: Rect, app: &App) {
    // Clear the transcript beneath so the overlay reads as a popup, not inline
    // text bleeding through the rows (same pattern as the approval card).
    f.render_widget(Clear, area);
    let dim = Style::new().fg(Color::DarkGray);
    let cursor = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let n = app.pending.len();
    let focus = if n == 0 {
        0
    } else {
        app.queue_focus.min(n - 1)
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        " queue  (e edit \u{00b7} d del \u{00b7} a all \u{00b7} Ctrl+G/Esc close)",
        dim,
    )));
    lines.push(Line::raw(""));
    for (i, item) in app.pending.iter().enumerate() {
        let is_focus = i == focus;
        let style = if is_focus { cursor } else { dim };
        let marker = if is_focus { "\u{276f} " } else { "  " };
        let prefix = format!("{marker}{} ", i + 1);
        let avail = (area.width as usize).saturating_sub(prefix.chars().count());
        let body = truncate_chars(item.display(), avail);
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(body, style),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " \u{2191}\u{2193} move \u{00b7} e recall \u{00b7} d del \u{00b7} a recall all \u{00b7} Ctrl+G/Esc close",
        dim,
    )));
    f.render_widget(Paragraph::new(lines), area);
}

/// Bounded height of the ambient queued-input footer strip (the sibling of
/// the Ctrl+G overlay). Zero when the queue is empty; otherwise the number of
/// preview rows capped at two plus a +N more overflow line — and further
/// capped so a transcript floor (max of ten rows or half the window) always
/// survives beneath it. Below twenty rows the strip collapses to a one-line
/// summary so the interaction view is never buried.
pub(super) fn strip_height(app: &App, total_h: u16, input_h: u16) -> u16 {
    let n = app
        .pending
        .iter()
        .filter(|s| !s.display().is_empty())
        .count();
    if n == 0 {
        return 0;
    }
    if total_h < 20 {
        return 1;
    }
    let want = std::cmp::min(n, 2) as u16 + u16::from(n > 2);
    let floor = std::cmp::max(10, total_h / 2);
    let budget = total_h
        .saturating_sub(input_h)
        .saturating_sub(1)
        .saturating_sub(floor);
    // Never fully hide a non-empty queue: if the transcript-floor budget
    // collapsed to zero, fall back to the one-line summary so ambient
    // awareness survives (one row is cheaper than losing the queue entirely).
    want.min(budget).max(1)
}

/// Render the read-only ambient queued-input strip below the input box: the
/// most recent pending items as dim single-line previews plus a +N more
/// overflow, or a one-line summary when the window is too small. Per-item
/// edit/delete is the Ctrl+G overlay (not this strip).
pub(super) fn draw_strip(f: &mut Frame, area: Rect, app: &App) {
    let items: Vec<_> = app
        .pending
        .iter()
        .filter(|s| !s.display().is_empty())
        .collect();
    if items.is_empty() {
        app.queue_rect.set(Rect::new(0, 0, 0, 0));
        return;
    }
    // Stash the strip rect so mouse clicks can map to a queued item.
    app.queue_rect.set(area);
    let dim = Style::new().fg(Color::DarkGray);
    let mut lines: Vec<Line> = Vec::new();
    // One row (small window OR budget-constrained): the summary form, EXCEPT
    // when there is exactly one item — a single preview fits one row and is
    // more useful than +1 queued. More rows: up to two item previews, the
    // first carrying the Ctrl+G manager hint (always on while the queue is
    // non-empty, so a 1- or 2-item queue still surfaces the edit/delete entry),
    // plus a +N more overflow line when it overflows.
    let one_row_summary = area.height <= 1 && items.len() > 1;
    if one_row_summary {
        lines.push(Line::from(Span::styled(
            format!("⏵ +{} queued (Ctrl+G to manage)", items.len()),
            dim,
        )));
    } else {
        let cap = if area.height <= 1 { 1 } else { 2 };
        let shown = std::cmp::min(items.len(), cap);
        for (i, item) in items.iter().take(shown).enumerate() {
            let hint = if i == 0 { " (Ctrl+G to manage)" } else { "" };
            lines.push(Line::from(Span::styled(
                format!("⏵ queued: {}{}", item.display(), hint),
                dim,
            )));
        }
        let more = items.len() - shown;
        if more > 0 {
            lines.push(Line::from(Span::styled(format!("  +{} more", more), dim)));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}
