//! Level 0 of the trajectory pane: the session summary header and the turn
//! list body.
//!
//! The list is the pane's index surface, so it owns the turn title, the
//! column layout, and the boundary separator. The drill-down renderers live in
//! the detail submodule.

use super::*;

/// The display title for a turn: the user input when present, otherwise the
/// first event's summary so a turn whose prompt sits outside the loaded window
/// still names itself. When there is nothing to derive from, "(no input)"
/// surfaces.
pub(super) fn turn_title(turn: &TrajectoryTurn) -> String {
    if !turn.user_input.trim().is_empty() {
        return turn.user_input.clone();
    }
    match turn.records.first() {
        Some(first) if !first.summary.trim().is_empty() => first.summary.clone(),
        _ => "(no input)".to_string(),
    }
}

// Rendering

/// The body lines for one turn: a context-cleared separator when the log
/// carries one, then the row itself. The separator is a label rather than a
/// row, so it never takes a cursor position.
pub(super) fn turn_row(
    t: &TrajectoryTurn,
    selected: bool,
    width: usize,
    show_per_turn_model: bool,
    now_secs: u64,
) -> Vec<Line<'static>> {
    let prefix = if selected { "▸ " } else { "  " };
    let mut out = Vec::new();
    if let Some(TurnBoundary::ContextCleared { at_secs, .. }) = t.boundary_before {
        out.push(line(vec![sp(
            format!(
                "  ── context cleared · {} ──",
                relative_time(now_secs, at_secs)
            ),
            Color::DarkGray,
        )]));
    }
    let glyph = if t.success { "✓" } else { "✗" };
    let gc = if t.success { Color::Green } else { Color::Red };
    let tokens = format!(
        "{} in {} out",
        fmt_k_opt(t.tokens_in),
        fmt_k_opt(t.tokens_out)
    );
    let cached = match (t.cache_read, t.tokens_in) {
        (Some(c), Some(tin)) if tin > 0 && c > 0 => {
            format!("{:.0}% cached", 100.0 * c as f64 / tin as f64)
        }
        _ => String::new(),
    };
    // Thinking tokens are a component of output, so the parenthetical sits
    // tight against the output number and carries its own inclusion note.
    let thinking = match t.reasoning_tokens {
        Some(r) if r > 0 => format!("(thinking {})", fmt_k(r)),
        _ => String::new(),
    };
    // Per-turn model and effort only when the session saw at least two distinct
    // models: one id repeated on every row is noise. A turn that switched
    // models lists each id it used.
    let model = if show_per_turn_model && !t.models.is_empty() {
        t.models.join(",")
    } else {
        String::new()
    };
    let effort = if show_per_turn_model && !t.efforts.is_empty() {
        t.efforts.join(",")
    } else {
        String::new()
    };
    let calls = if t.tool_count > 0 {
        format!("{} calls", t.tool_count)
    } else {
        "direct".to_string()
    };
    // Only a turn that had a failure states one; a zero would be noise on every
    // clean row.
    let fails = if t.tool_fail > 0 {
        format!("{} fail", t.tool_fail)
    } else {
        String::new()
    };
    // Columns are dropped from the least informative end as the terminal
    // narrows, so the duration and the outcome always survive. Padding counts
    // display columns, not characters: a wide glyph would otherwise shift every
    // column after it.
    let mut spans = vec![
        sp(prefix, Color::Cyan),
        sp(pad(&format!("T{}", t.n), 5), Color::Cyan),
        sp(pad(&truncate_width(&turn_title(t), 30), 30), Color::White),
        sp(pad(&tokens, 15), Color::Gray),
        sp(pad(&cached, 12), Color::Indexed(208)),
    ];
    if width >= 124 {
        spans.push(sp(pad(&thinking, 16), Color::DarkGray));
    }
    if width >= 148 && show_per_turn_model {
        spans.push(sp(pad(&truncate_width(&model, 20), 20), Color::DarkGray));
        spans.push(sp(pad(&truncate_width(&effort, 8), 8), Color::DarkGray));
    }
    if width >= 96 {
        spans.push(sp(pad(&calls, 9), Color::Gray));
        spans.push(sp(
            pad(&fails, 8),
            if t.tool_fail > 0 {
                Color::Red
            } else {
                Color::DarkGray
            },
        ));
    }
    spans.push(sp(
        format!("{:>6.1}s ", t.duration_ms as f64 / 1000.0),
        Color::Gray,
    ));
    spans.push(sp(glyph, gc));
    out.push(line(spans));
    out
}

/// Level 0: session summary header + turn list body. The cursor selects a row
/// (a turn or a background event); Enter drills into the focused turn.
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
pub(super) fn draw_turn_list(
    traj: &TrajectoryView,
    cursor: usize,
    area: Rect,
) -> (
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    usize,
) {
    let total_calls: usize = traj
        .rows
        .iter()
        .map(|r| match r {
            TrajectoryRow::Turn(t) => t.tool_count,
            _ => 0,
        })
        .sum();
    let tokens_summary = match (traj.tokens_in, traj.tokens_out) {
        (Some(tin), Some(tout)) => format!("{} in {} out", fmt_k(tin), fmt_k(tout)),
        (Some(tin), None) => format!("{} in", fmt_k(tin)),
        (None, Some(tout)) => format!("{} out", fmt_k(tout)),
        (None, None) => "—".to_string(),
    };
    let cache_hit_str = match (traj.cache_read, traj.tokens_in) {
        (Some(c), Some(tin)) if tin > 0 => {
            format!(" · cache hit {:.0}%", 100.0 * c as f64 / tin as f64)
        }
        _ => String::new(),
    };
    let turns_label = if traj.hidden_turns > 0 {
        format!(
            "{} turns ({} older not loaded)",
            traj.total_turns, traj.hidden_turns
        )
    } else {
        format!("{} turns", traj.total_turns)
    };
    let mut header = vec![line(vec![
        sp(turns_label, Color::Cyan),
        sp(" · ", Color::DarkGray),
        sp(tokens_summary, Color::Gray),
        sp(cache_hit_str, Color::Indexed(208)),
        sp(format!(" · {} calls", total_calls), Color::Gray),
        sp(format!(" · {} fail", traj.failures), Color::Red),
        sp(format!(" · total {}s", traj.duration_secs), Color::Gray),
    ])];
    let mut timing_spans = Vec::new();
    if let Some(avg) = traj.timing.ttft_avg_ms {
        timing_spans.push(sp(
            format!("TTFT avg {:.1}s", avg as f64 / 1000.0),
            Color::DarkGray,
        ));
    }
    if let Some(p95) = traj.timing.ttft_p95_ms {
        if !timing_spans.is_empty() {
            timing_spans.push(sp(" · ", Color::DarkGray));
        }
        timing_spans.push(sp(
            format!("p95 {:.1}s", p95 as f64 / 1000.0),
            Color::DarkGray,
        ));
    }
    if let Some(p99) = traj.timing.ttft_p99_ms {
        if !timing_spans.is_empty() {
            timing_spans.push(sp(" · ", Color::DarkGray));
        }
        timing_spans.push(sp(
            format!("p99 {:.1}s", p99 as f64 / 1000.0),
            Color::DarkGray,
        ));
    }
    if let Some(tps) = traj.timing.decode_tok_per_sec {
        if !timing_spans.is_empty() {
            timing_spans.push(sp(" · ", Color::DarkGray));
        }
        timing_spans.push(sp(format!("decode {:.1} tok/s", tps), Color::DarkGray));
    }
    if !timing_spans.is_empty() {
        timing_spans.push(sp(format!(" · {}", traj.model), Color::DarkGray));
        header.push(line(timing_spans));
    } else {
        header.push(line(vec![sp(traj.model.clone(), Color::DarkGray)]));
    }
    header.push(blank());
    // Per-turn model/effort attribution: render only when the session saw
    // ≥2 distinct model ids (otherwise every row would repeat the same id
    // — noise, not signal). When ≥2, each turn that used a model shows them.
    let show_per_turn_model = traj
        .rows
        .iter()
        .filter_map(|r| match r {
            TrajectoryRow::Turn(t) => Some(t.models.iter().map(String::as_str)),
            _ => None,
        })
        .flatten()
        .collect::<HashSet<_>>()
        .len()
        >= 2;
    let mut body = Vec::new();
    let mut sel_line = 0usize;
    let clamped = cursor.min(traj.rows.len().saturating_sub(1));
    let now_secs = now_epoch_secs();
    let width = area.width as usize;
    for (i, row) in traj.rows.iter().enumerate() {
        let sel = i == clamped;
        let prefix = if sel { "▸ " } else { "  " };
        match row {
            TrajectoryRow::Turn(t) => {
                for extra in turn_row(t, sel, width, show_per_turn_model, now_secs) {
                    body.push(extra);
                }
                // The selected row's body line is taken after any separator, so
                // the scroll offset follows the row the user actually sees.
                if sel {
                    sel_line = body.len() - 1;
                }
            }
            TrajectoryRow::Bg(bg) => {
                if sel {
                    sel_line = body.len();
                }
                body.push(line(vec![
                    sp(prefix, Color::Cyan),
                    sp("[bg] ", Color::DarkGray),
                    sp(format!("{:8} ", bg.kind), Color::DarkGray),
                    sp(truncate_width(&bg.summary, 50), Color::DarkGray),
                    sp(
                        format!("  {:.1}s", bg.duration_ms as f64 / 1000.0),
                        Color::DarkGray,
                    ),
                ]));
            }
        }
    }
    let footer = vec![
        blank(),
        key_hint(&[
            ("Up/Down", "select"),
            ("Home/End", "top/end"),
            ("Enter", "open"),
            ("Esc", "close"),
        ]),
    ];
    (header, body, footer, sel_line)
}

/// Pad a string to a display width, counting columns rather than characters
/// so a wide glyph cannot shift the columns after it.
pub(super) fn pad(text: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(text);
    if w >= width {
        return text.to_string();
    }
    let mut out = text.to_string();
    out.push_str(&" ".repeat(width - w));
    out
}
