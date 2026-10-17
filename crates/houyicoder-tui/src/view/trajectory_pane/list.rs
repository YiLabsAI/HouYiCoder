//! Level 0 of the trajectory pane: the session summary header and the turn
//! list body.
//!
//! The list is the pane's index surface, so it owns the turn title, the
//! column layout, and the boundary separator. The drill-down renderers live in
//! the detail submodule.

use super::*;
use crate::command::render::format_tokens;
use crate::view::relative_time::{format_span_ms, session_span_label};

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

// Column layout

/// The turn label column, in display columns. The turn ordinal is bounded by
/// the session's own length, so a constant covers every row.
const TURN_LABEL_W: usize = 5;

/// The title column, in display columns.
const TURN_TITLE_W: usize = 30;

/// The cached-ratio cell's own widest form. The share behind it is a rate held
/// to one, so the figure is at most a three-digit percentage, whatever the
/// session's size.
const CACHED_CELL_W: usize = "100% cached".len();

/// The model and effort columns, in display columns.
const MODEL_W: usize = 20;
const EFFORT_W: usize = 8;

/// The gap a column leaves after its content, so two columns never touch.
const COLUMN_GAP_W: usize = 2;

/// The cached-ratio column, in display columns: the widest cell plus the
/// trailing gap, so the widest ratio does not run into the column after it.
const CACHED_W: usize = CACHED_CELL_W + COLUMN_GAP_W;

/// The part of the token cell's width that does not depend on the figures: the
/// two keywords and the spaces that separate them from their numbers.
const TOKENS_FIXED_W: usize = " in ".len() + " out".len();

/// The terminal widths at which the optional columns join. Below them the
/// optional columns are dropped from the least informative end, leaving the
/// turn, its tokens, its duration, and its outcome. Those four fixed columns
/// take about sixty columns between them, so a terminal narrower than that
/// clips the row and no gate here applies.
const CALLS_MIN_WIDTH: usize = 96;
const THINKING_MIN_WIDTH: usize = 124;
const MODEL_MIN_WIDTH: usize = 148;

/// A count with its noun, singular at one: a single call reads as a count, not
/// as the plural template with the number dropped into it.
fn count_label(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// The share of this turn's input that came from the cache, or nothing when the
/// turn states no share. The turn's own derivation decides that: a count the
/// endpoint never reported, and an account missing one of the two numbers the
/// share is divided from, both leave the cell blank rather than reading as a
/// measured miss.
fn cached_cell(t: &TrajectoryTurn) -> String {
    match t.cache_hit_pct() {
        Some(pct) => format!("{pct:.0}% cached"),
        None => String::new(),
    }
}

/// Thinking tokens are a component of output, so the parenthetical sits tight
/// against the output number and carries its own inclusion note. The time the
/// turn spent thinking is stated beside them, because a turn that thought for
/// seconds is the one a reader is looking for.
///
/// Each figure is stated for what the log holds and neither invents the other.
/// A span the calls reported is stated whatever it measured, a block closed
/// inside a millisecond reading as zero; a count of zero stays off the line,
/// because a reasoning count the endpoint left at zero cannot be told from one
/// it never reported.
fn thinking_cell(t: &TrajectoryTurn) -> String {
    let tokens = match t.reasoning_tokens {
        Some(r) if r > 0 => Some(format_tokens(r as u64)),
        _ => None,
    };
    let span = t.reasoning_ms.map(format_span_ms);
    match (span, tokens) {
        (Some(span), Some(tokens)) => format!("(thinking {span} · {tokens})"),
        (Some(span), None) => format!("(thinking {span})"),
        (None, Some(tokens)) => format!("(thinking {tokens})"),
        (None, None) => String::new(),
    }
}

/// The model ids a turn used. One id repeated on every row is noise, so the
/// column is drawn only when the session saw at least two distinct models.
fn model_cell(t: &TrajectoryTurn) -> String {
    t.models.join(",")
}

/// The effort levels a turn ran at, on the same condition as the model ids.
fn effort_cell(t: &TrajectoryTurn) -> String {
    t.efforts.join(",")
}

/// The turn's tool calls, or that it made none.
fn calls_cell(t: &TrajectoryTurn) -> String {
    if t.tool_count > 0 {
        count_label(t.tool_count, "call")
    } else {
        "direct".to_string()
    }
}

/// Only a turn that had a failure states one; a zero would be noise on every
/// clean row.
fn fails_cell(t: &TrajectoryTurn) -> String {
    if t.tool_fail > 0 {
        count_label(t.tool_fail, "fail")
    } else {
        String::new()
    }
}

/// The widest cell one column carries across the rows on screen.
fn max_cell_width<F: Fn(&TrajectoryTurn) -> String>(turns: &[&TrajectoryTurn], cell: F) -> usize {
    turns
        .iter()
        .map(|t| UnicodeWidthStr::width(cell(t).as_str()))
        .max()
        .unwrap_or(0)
}

/// The column widths for one render of the turn list.
///
/// A table's columns are one decision for the whole screen rather than a
/// per-row choice: a cell wider than its column pushes every column after it
/// out of line, which reads as two rows that disagree. The columns whose
/// figures grow with the session are therefore sized from the widest cell on
/// screen, and a column no visible row fills is dropped, because a column of
/// blanks costs the row width while saying nothing.
pub(super) struct TurnColumns {
    /// The token cell's two subcolumns: one cell on the row, sized as two, so
    /// the figures line up under each other instead of drifting with the width
    /// of the figure beside them. The suffix keeps each width apart from the
    /// count of the same name on the turn.
    in_width: usize,
    out_width: usize,
    /// The whole token cell: both subcolumns plus the keywords between them.
    tokens_width: usize,
    cached: bool,
    thinking: Option<usize>,
    model: bool,
    calls: Option<usize>,
    fails: Option<usize>,
}

impl TurnColumns {
    /// The widths for the rows about to be drawn at this terminal width.
    fn for_rows(rows: &[TrajectoryRow], width: usize, show_per_turn_model: bool) -> Self {
        let turns: Vec<&TrajectoryTurn> = rows
            .iter()
            .filter_map(|row| match row {
                TrajectoryRow::Turn(t) => Some(t),
                TrajectoryRow::Bg(_) => None,
            })
            .collect();
        let in_width = max_cell_width(&turns, |t| tokens_or_dash(t.tokens_in));
        let out_width = max_cell_width(&turns, |t| tokens_or_dash(t.tokens_out));
        let filled = |cell: fn(&TrajectoryTurn) -> String| -> Option<usize> {
            match max_cell_width(&turns, cell) {
                0 => None,
                w => Some(w + COLUMN_GAP_W),
            }
        };
        TurnColumns {
            in_width,
            out_width,
            tokens_width: in_width + out_width + TOKENS_FIXED_W + COLUMN_GAP_W,
            cached: max_cell_width(&turns, cached_cell) > 0,
            thinking: if width >= THINKING_MIN_WIDTH {
                filled(thinking_cell)
            } else {
                None
            },
            model: width >= MODEL_MIN_WIDTH && show_per_turn_model,
            calls: if width >= CALLS_MIN_WIDTH {
                filled(calls_cell)
            } else {
                None
            },
            fails: if width >= CALLS_MIN_WIDTH {
                filled(fails_cell)
            } else {
                None
            },
        }
    }

    /// Both token counts, or an em dash for a count the turn never reported,
    /// each figure right-aligned in its own subcolumn.
    fn tokens_cell(&self, t: &TrajectoryTurn) -> String {
        format!(
            "{:>in_w$} in {:>out_w$} out",
            tokens_or_dash(t.tokens_in),
            tokens_or_dash(t.tokens_out),
            in_w = self.in_width,
            out_w = self.out_width,
        )
    }
}

/// The body lines for one turn: a context-cleared separator when the log
/// carries one, then the row itself. The separator is a label rather than a
/// row, so it never takes a cursor position.
///
/// Padding counts display columns, not characters: a wide glyph would otherwise
/// shift every column after it. A truncated cell is capped two columns below its
/// width, because the truncation spends one of them on the ellipsis and the next
/// cell needs the other as its gap.
pub(super) fn turn_row(
    t: &TrajectoryTurn,
    selected: bool,
    cols: &TurnColumns,
    now_secs: u64,
) -> Vec<Line<'static>> {
    let prefix = if selected { "▸ " } else { "  " };
    let mut out = Vec::new();
    for boundary in &t.boundary_before {
        out.push(boundary_line(boundary, now_secs));
    }
    let glyph = if t.success { "✓" } else { "✗" };
    let gc = if t.success { Color::Green } else { Color::Red };
    let mut spans = vec![
        sp(prefix, Color::Cyan),
        sp(pad(&format!("T{}", t.n), TURN_LABEL_W), Color::Cyan),
        sp(
            pad(&truncate_width(&t.title, TURN_TITLE_W - 2), TURN_TITLE_W),
            Color::White,
        ),
        sp(pad(&cols.tokens_cell(t), cols.tokens_width), Color::Gray),
    ];
    if cols.cached {
        spans.push(sp(pad(&cached_cell(t), CACHED_W), Color::Indexed(208)));
    }
    if let Some(w) = cols.thinking {
        spans.push(sp(pad(&thinking_cell(t), w), Color::DarkGray));
    }
    if cols.model {
        spans.push(sp(
            pad(&truncate_width(&model_cell(t), MODEL_W - 2), MODEL_W),
            Color::DarkGray,
        ));
        spans.push(sp(
            pad(&truncate_width(&effort_cell(t), EFFORT_W - 2), EFFORT_W),
            Color::DarkGray,
        ));
    }
    if let Some(w) = cols.calls {
        spans.push(sp(pad(&calls_cell(t), w), Color::Gray));
    }
    if let Some(w) = cols.fails {
        spans.push(sp(
            pad(&fails_cell(t), w),
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
    BodyScroll,
) {
    let tokens_summary = match (traj.tokens_in, traj.tokens_out) {
        (Some(tin), Some(tout)) => format!(
            "{} in {} out",
            format_tokens(tin as u64),
            format_tokens(tout as u64)
        ),
        (Some(tin), None) => format!("{} in", format_tokens(tin as u64)),
        (None, Some(tout)) => format!("{} out", format_tokens(tout as u64)),
        (None, None) => "—".to_string(),
    };
    let cache_hit_str = match (traj.cache_read, traj.tokens_in) {
        (Some(c), Some(tin)) => traj
            .usage_provenance
            .cache_hit_pct(c, tin as u64)
            .map(|pct| format!(" · cache hit {pct:.0}%"))
            .unwrap_or_default(),
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
        count_label(traj.total_turns, "turn")
    } else {
        format!(
            "{} ({})",
            count_label(traj.total_turns, "turn"),
            not_loaded.join(", ")
        )
    };
    let mut header = vec![line(vec![
        sp(turns_label, Color::Cyan),
        sp(" · ", Color::DarkGray),
        sp(tokens_summary, Color::Gray),
        sp(cache_hit_str, Color::Indexed(208)),
        sp(
            format!(" · {}", count_label(traj.tool_calls, "call")),
            Color::Gray,
        ),
        // A failure count is a fact, not an alarm: red is kept for a session
        // that actually had failures, so a clean session's header does not
        // shout the zero on every frame.
        sp(
            format!(" · {}", count_label(traj.failures, "fail")),
            if traj.failures > 0 {
                Color::Red
            } else {
                Color::DarkGray
            },
        ),
        sp(
            format!(" · total {}", session_span_label(traj.duration_ms)),
            Color::Gray,
        ),
    ])];
    if traj.skipped_records > 0 {
        header.push(line(vec![sp(
            format!(
                "  {} skipped",
                count_label(traj.skipped_records, "unreadable record")
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
            return (
                header,
                body,
                vec![blank(), key_hint(&[("Esc", "back")])],
                BodyScroll::Cursor(0),
            );
        }
        TrajectoryViewState::Failed => {
            body.push(line(vec![sp(
                "  could not read trajectory history",
                Color::Red,
            )]));
            return (
                header,
                body,
                vec![blank(), key_hint(&[("Esc", "back")])],
                BodyScroll::Cursor(0),
            );
        }
        // Older turns are on their way and the rows already loaded stay: the
        // line is an addition to the list, not a replacement for it.
        TrajectoryViewState::LoadingOlder => {
            body.push(line(vec![sp("  loading older turns...", Color::DarkGray)]));
        }
        // A session that has run nothing yet is ready and empty, which is a
        // fact about the session rather than a read still running: say so
        // instead of leaving an area under the header that reads as a load
        // failure.
        TrajectoryViewState::Ready => {
            if traj.rows.is_empty() {
                body.push(line(vec![sp("  no turns recorded yet", Color::DarkGray)]));
            }
        }
    }
    let clamped = cursor.min(traj.rows.len().saturating_sub(1));
    let now_secs = now_epoch_secs();
    let width = area.width as usize;
    let cols = TurnColumns::for_rows(&traj.rows, width, show_per_turn_model);
    for (i, row) in traj.rows.iter().enumerate() {
        let sel = i == clamped;
        let prefix = if sel { "▸ " } else { "  " };
        match row {
            TrajectoryRow::Turn(t) => {
                for extra in turn_row(t, sel, &cols, now_secs) {
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
    (header, body, footer, BodyScroll::Cursor(sel_line))
}
