//! Level 0 of the trajectory pane: the session summary header and the turn
//! list body.
//!
//! The list is the pane's index surface, so it owns the turn title, the
//! column layout, and the boundary separator. The drill-down renderers live in
//! the detail submodule.

use super::*;
use crate::command::render::format_tokens;
use crate::view::relative_time::{format_span_ms, format_span_secs};

// Rendering

/// Render a boundary separator between turns, when the turn carried one.
fn boundary_line(boundary: &TurnBoundary, now_secs: u64) -> Line<'static> {
    match boundary {
        TurnBoundary::ContextCleared { at_secs, .. } => line(vec![sp(
            format!(
                "  ── context cleared · {} ──",
                relative_time(now_secs, *at_secs)
            ),
            Color::DarkGray,
        )]),
        TurnBoundary::ModelSwitch(b) => line(vec![sp(
            format!(
                "  ── model {} → {} · {} ──",
                b.from,
                b.to,
                relative_time(now_secs, b.at_secs)
            ),
            Color::DarkGray,
        )]),
        TurnBoundary::Compacted(b) => {
            // The bracket is the point of the row: a compaction that reclaimed
            // nothing is worth seeing. A log written before the counts were
            // recorded carries zeroes, so the bracket is omitted there rather
            // than printed as a fold from nothing to nothing.
            let bracket = if b.pre_tokens > 0 || b.post_tokens > 0 {
                format!(
                    " {} → {}",
                    format_tokens(b.pre_tokens),
                    format_tokens(b.post_tokens)
                )
            } else {
                String::new()
            };
            line(vec![sp(
                format!(
                    "  ── context compacted{bracket} (checkpoint {}) · {} ──",
                    b.checkpoint_id,
                    relative_time(now_secs, b.at_secs)
                ),
                Color::DarkGray,
            )])
        }
    }
}

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
    for boundary in &t.boundary_before {
        out.push(boundary_line(boundary, now_secs));
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
        format!(
            "{} fail{}",
            t.tool_fail,
            if t.tool_fail == 1 { "" } else { "s" }
        )
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
        sp(pad(&truncate_width(&t.title, 30), 30), Color::White),
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
        format!("{:>7} ", format_span_ms(t.duration_ms)),
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
    // Both ends can be unloaded at once: the window is walked back from the
    // tail, so the turns it dropped are newer than the ones it shows, not
    // only older ones behind it.
    let mut not_loaded = Vec::new();
    if traj.hidden_turns > 0 {
        not_loaded.push(format!("{} older not loaded", traj.hidden_turns));
    }
    if traj.newer_hidden > 0 {
        not_loaded.push(format!("{} newer not loaded", traj.newer_hidden));
    }
    let turns_label = if not_loaded.is_empty() {
        format!("{} turns", traj.total_turns)
    } else {
        format!("{} turns ({})", traj.total_turns, not_loaded.join(", "))
    };
    let mut header = vec![line(vec![
        sp(turns_label, Color::Cyan),
        sp(" · ", Color::DarkGray),
        sp(tokens_summary, Color::Gray),
        sp(cache_hit_str, Color::Indexed(208)),
        sp(format!(" · {} calls", traj.tool_calls), Color::Gray),
        sp(
            format!(
                " · {} fail{}",
                traj.failures,
                if traj.failures == 1 { "" } else { "s" }
            ),
            Color::Red,
        ),
        sp(
            format!(" · total {}", format_span_secs(traj.duration_secs)),
            Color::Gray,
        ),
    ])];
    if traj.skipped_records > 0 {
        header.push(line(vec![sp(
            format!(
                "  {} unreadable record{} skipped",
                traj.skipped_records,
                if traj.skipped_records == 1 { "" } else { "s" }
            ),
            Color::Yellow,
        )]));
    }
    let mut timing_spans = Vec::new();
    if let Some(avg) = traj.timing.ttft_avg_ms {
        timing_spans.push(sp(
            format!("TTFT avg {}", format_span_ms(avg)),
            Color::DarkGray,
        ));
    }
    if let Some(p95) = traj.timing.ttft_p95_ms {
        if !timing_spans.is_empty() {
            timing_spans.push(sp(" · ", Color::DarkGray));
        }
        timing_spans.push(sp(format!("p95 {}", format_span_ms(p95)), Color::DarkGray));
    }
    if let Some(p99) = traj.timing.ttft_p99_ms {
        if !timing_spans.is_empty() {
            timing_spans.push(sp(" · ", Color::DarkGray));
        }
        timing_spans.push(sp(format!("p99 {}", format_span_ms(p99)), Color::DarkGray));
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
    // From the session's own model count, not from the rows in hand: a window
    // holding one model would otherwise hide the column on a session that
    // switched, while the header says how many it used.
    let show_per_turn_model = traj.models_used >= 2;
    let mut body = Vec::new();
    let mut sel_line = 0usize;
    // A read that has not landed is its own state. Rendering an empty list
    // would say the session has no turns, and the demonstration rows would say
    // it has turns it does not.
    match traj.state {
        // Nothing truthful to list yet, so the body says so instead of showing
        // an empty table or the demonstration rows.
        TrajectoryViewState::Loading => {
            body.push(line(vec![sp("  loading trajectory...", Color::DarkGray)]));
            return (header, body, vec![blank(), key_hint(&[("Esc", "back")])], 0);
        }
        TrajectoryViewState::Failed => {
            body.push(line(vec![sp(
                "  could not read trajectory history",
                Color::Red,
            )]));
            return (header, body, vec![blank(), key_hint(&[("Esc", "back")])], 0);
        }
        // Older turns are on their way and the rows already loaded stay: the
        // line is an addition to the list, not a replacement for it.
        TrajectoryViewState::LoadingOlder => {
            body.push(line(vec![sp("  loading older turns...", Color::DarkGray)]));
        }
        TrajectoryViewState::Ready => {}
    }
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
                        format!("  {}", format_span_ms(bg.duration_ms)),
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
