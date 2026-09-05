//! Ambient queued-input strip render. Sits above the input box, read-only,
//! showing pending items with state glyphs. See keys::handle_working for the
//! dispatch and run_control::pop_queued_to_input for the Esc recall path.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::pending_queue::PendingItem;
use crate::state::App;

/// Rows the ambient queued-input strip would like, capped at two: the head
/// item plus, on overflow, a "+N more" summary row. Zero when the queue is
/// empty. What it actually gets comes from the shared footer budget, which
/// weighs it against the other strips; with one row the strip draws its
/// one-line summary.
pub(super) fn strip_want(app: &App) -> u16 {
    let n = app
        .pending
        .iter()
        .filter(|s| !s.display().is_empty())
        .count();
    if n == 0 {
        return 0;
    }
    std::cmp::min(n, 2) as u16
}

/// Render the read-only queued-input strip above the input box. The glyph
/// keys on position + the drain gate, not the item's type: a non-head
/// ParkedMessage may still auto-run (queued behind the live head) or not
/// (orphaned by an interrupt), and the type cannot tell those apart.
/// Gate open (busy or clean idle): head Message -> "→ next", non-head ->
/// "· n.". Gate closed (idle after a non-final end): all -> "⏸ held".
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
    // Drain gate: open while busy or after a clean run end; closed when idle
    // after a non-final end (interrupt/error), so the queue parks.
    let gate_closed = !app.agent_busy && !app.status.last_run_final;
    let mut lines: Vec<Line> = Vec::new();
    let one_row_summary = area.height <= 1 && items.len() > 1;
    if one_row_summary {
        let head = if gate_closed { "⏸" } else { "→" };
        lines.push(Line::from(Span::styled(
            format!("{head} +{}", items.len()),
            dim,
        )));
    } else {
        let cap = if area.height <= 1 { 1 } else { 2 };
        // With overflow, drop to one real row so the "+N more" summary fits
        // within the cap; the count conveys scale a second preview cannot.
        let shown = if items.len() > cap { 1 } else { items.len() };
        for (i, item) in items.iter().take(shown).enumerate() {
            let (glyph, label) = if gate_closed {
                ("⏸", "held".to_string())
            } else if i == 0 && matches!(item, PendingItem::Message(_)) {
                ("→", "next".to_string())
            } else {
                ("·", format!("{}.", i + 1))
            };
            lines.push(Line::from(Span::styled(
                format!("{glyph} {label}  {}", item.display()),
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
