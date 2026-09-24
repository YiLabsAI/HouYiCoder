//! Level 1 and Level 2 drill-down renderers for the trajectory pane, and what
//! the pane knows about the turn it is showing.
//!
//! Level 1 draws one turn's record timeline; Level 2 draws the full detail of
//! the selected record. Both share the pane's line helpers and view types.

use super::*;

/// What the pane knows about the turn its drill is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TrajectoryDetailState {
    /// Asked for, not answered yet.
    #[default]
    Loading,
    /// The turn's records are in hand.
    Ready,
    /// The read failed, and the pane says so rather than showing an empty turn.
    Failed,
    /// The turn's history was cleared, so the key names a turn of a history
    /// that is no longer the session's.
    Stale,
}

/// One turn's records, as the drill levels render them.
///
/// The turn's own facts stay on its row: a drill asks for the records, and the
/// row it came from already answers what the turn was and what it spent.
#[derive(Clone, Default)]
pub struct TrajectoryDetailView {
    pub state: TrajectoryDetailState,
    /// The turn's records in log order.
    pub records: Vec<TrajectoryRecord>,
    /// True when the turn was wider than one detail read, so the records shown
    /// are its beginning. A silent truncation would present part of a turn as
    /// the whole of it.
    pub truncated: bool,
}
use crate::view::relative_time::format_span_ms;

/// The colour a record's outcome is drawn in.
fn outcome_color(outcome: RecordOutcome) -> Color {
    match outcome {
        RecordOutcome::Ok => Color::Green,
        RecordOutcome::Failed => Color::Red,
        RecordOutcome::Pending => Color::DarkGray,
    }
}

/// Level 1: turn title header + a positional Gantt timeline of the turn's
/// records. Each row's bar sits at its start offset on the shared turn time
/// axis (width = duration), so records that overlapped in time overlap on the
/// same columns and the latency hot-spots are visible at a glance. A ruler line
/// orients the scale, and the kind and name columns say what each row was.
pub(super) fn draw_turn_detail(
    row: &TrajectoryRow,
    detail: &TrajectoryDetailView,
    cursor: usize,
    area: Rect,
    app: &crate::state::App,
) -> (
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    usize,
) {
    let mut header = Vec::new();
    let mut body = Vec::new();
    match row.clone() {
        TrajectoryRow::Turn(turn) => {
            app.trajectory.set_at_bg(false);
            let clamped = cursor.min(detail.records.len().saturating_sub(1));
            let cache_str = format_turn_cache(&turn);
            header.push(line(vec![
                sp(
                    format!(" T{}  \"{}\"", turn.n, truncate_width(&turn.title, 30)),
                    Color::Cyan,
                ),
                sp(
                    format!(
                        "  {} in {} out{} · total {}",
                        fmt_k_opt(turn.tokens_in),
                        fmt_k_opt(turn.tokens_out),
                        cache_str,
                        format_span_ms(turn.duration_ms)
                    ),
                    Color::Gray,
                ),
            ]));
            header.push(blank());
            // Layout, in display columns: the prefix, the bar, a gap, the
            // duration, the summary, and the outcome glyph. The bar takes what
            // is left after the fixed columns, so a narrow terminal shrinks
            // the timeline rather than the numbers.
            let bar_area = (area.width as usize)
                .saturating_sub(TIMELINE_PREFIX_W + TIMELINE_SUFFIX_W + TIMELINE_SUMMARY_MIN_W)
                .max(8);
            // No floor: on a narrow terminal the summary is what gives way, so
            // the row still ends with the duration and the outcome glyph.
            let summary_w = (area.width as usize)
                .saturating_sub(TIMELINE_PREFIX_W + bar_area + TIMELINE_SUFFIX_W);
            header.push(ruler_line(turn.duration_ms, bar_area));
            match detail.state {
                TrajectoryDetailState::Ready => {
                    for (i, ev) in detail.records.iter().enumerate() {
                        body.push(record_row(
                            ev,
                            i == clamped,
                            turn.duration_ms,
                            bar_area,
                            summary_w,
                        ));
                    }
                }
                TrajectoryDetailState::Loading => body.push(line(vec![sp(
                    "  reading this turn's records...",
                    Color::DarkGray,
                )])),
                TrajectoryDetailState::Failed => body.push(line(vec![sp(
                    "  could not read this turn's records",
                    Color::Red,
                )])),
                // The history was cleared under the drill: the key names a turn
                // that is not this session's to show.
                TrajectoryDetailState::Stale => return draw_drill_gone(),
            }
            if detail.truncated {
                body.push(line(vec![sp(
                    "  … this turn is longer than one read, showing its beginning",
                    Color::Yellow,
                )]));
            }
            let footer = vec![
                blank(),
                key_hint(&[("Up/Down", "select"), ("Enter", "open"), ("Esc", "back")]),
            ];
            (header, body, footer, clamped)
        }
        TrajectoryRow::Bg(bg) => {
            // A [bg] row drilled from L0 has no event timeline — show its
            // detail directly at L1 and flag it so Enter does not drill to L2.
            app.trajectory.set_at_bg(true);
            let mut bg_head = vec![
                sp(format!(" [bg] {} ", bg.kind), Color::Cyan),
                sp(truncate_width(&bg.summary, 50), Color::White),
            ];
            if bg.duration_ms > 0 {
                bg_head.push(sp(
                    format!("  {}", format_span_ms(bg.duration_ms)),
                    Color::Gray,
                ));
            }
            header.push(line(bg_head));
            header.push(blank());
            body.push(line(vec![
                sp(" kind: ", Color::DarkGray),
                sp(bg.kind.clone(), Color::Gray),
            ]));
            body.push(line(vec![
                sp(" summary: ", Color::DarkGray),
                sp(bg.summary.clone(), Color::White),
            ]));
            body.push(blank());
            // A span the log did not measure is absent rather than zero: a
            // latency of 0ms would claim a measurement that never happened.
            if bg.duration_ms > 0 {
                body.push(line(vec![
                    sp(" latency: ", Color::DarkGray),
                    sp(format_span_ms(bg.duration_ms), Color::Gray),
                ]));
            }
            let footer = vec![blank(), key_hint(&[("Esc", "back")])];
            (header, body, footer, 0)
        }
    }
}

/// What the drill levels show when the turn they were about is no longer in
/// the window: the history was cleared, or the window moved past it. Saying so
/// is the honest answer, because the row the drill froze now names another
/// turn.
pub(super) fn draw_drill_gone() -> (
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    usize,
) {
    (
        vec![blank()],
        vec![line(vec![sp(
            "  the turn this detail was about is no longer loaded",
            Color::DarkGray,
        )])],
        vec![blank(), key_hint(&[("Esc", "back")])],
        0,
    )
}

/// One Level 1 timeline row: the kind and name of the record, its bar on the
/// turn's time axis, its duration, and its summary with any measured latency
/// appended.
fn record_row(
    ev: &TrajectoryRecord,
    selected: bool,
    turn_ms: u64,
    bar_area: usize,
    summary_w: usize,
) -> Line<'static> {
    let prefix = if selected { "▸ " } else { "  " };
    let bar = positioned_bar(ev.start_ms, ev.duration_ms, turn_ms, bar_area);
    let bc = outcome_color(ev.outcome);
    let mark = ev.outcome.glyph();
    // The name column carries what the record acted on: the tool it called, the
    // agent it delegated to, the model it asked, prefixed by the call's ordinal
    // inside the turn.
    let name = ev.name.as_deref().unwrap_or("");
    let label = if ev.ordinal > 0 {
        format!("{} {name}", ev.ordinal)
    } else {
        name.to_string()
    };
    // A model call states its own measured split: the wait for the first token
    // and the decode rate of the tokens it produced. A part the log did not
    // record stays absent.
    let timing = match &ev.timing {
        Some(t) if ev.kind == TrajectoryRecordKind::Model => {
            let mut parts = Vec::new();
            if let Some(ttft) = t.ttft_ms {
                parts.push(format!("TTFT {ttft}ms"));
            }
            if let (Some(decode_ms), Some(out)) =
                (t.decode_ms, ev.usage.as_ref().and_then(|u| u.output))
                && decode_ms > 0
            {
                parts.push(format!(
                    "{:.1} tok/s",
                    out as f64 / (decode_ms as f64 / 1000.0)
                ));
            }
            if parts.is_empty() {
                String::new()
            } else {
                format!(" · {}", parts.join(" · "))
            }
        }
        _ => String::new(),
    };
    // The measured-latency suffix keeps its own columns and the summary takes
    // what is left, so a row never runs past the width it is drawn in. On a
    // terminal too narrow for both, the suffix is what goes: the summary is
    // what the row is about.
    let timing = if UnicodeWidthStr::width(timing.as_str()) < summary_w {
        timing
    } else {
        String::new()
    };
    let summary_w = summary_w.saturating_sub(UnicodeWidthStr::width(timing.as_str()));
    let summary = format!("{}{}", truncate_width(&ev.summary, summary_w), timing);
    line(vec![
        sp(prefix, Color::Cyan),
        sp(format!("{:7}", ev.kind.label()), Color::DarkGray),
        sp(" ", Color::DarkGray),
        // Padded by display columns, not characters: a wide glyph in the name
        // would otherwise shift the bar and everything after it.
        sp(
            pad(&truncate_width(&label, TIMELINE_NAME_W), TIMELINE_NAME_W),
            Color::Cyan,
        ),
        sp(bar, bc),
        sp(" ", Color::DarkGray),
        sp(
            format!(
                "{:>width$} ",
                format_span_ms(ev.duration_ms),
                width = TIMELINE_DUR_W - 1
            ),
            Color::Gray,
        ),
        sp(" ", Color::DarkGray),
        sp(summary, Color::White),
        sp(format!(" {}", mark), bc),
    ])
}

/// Level 2: the full detail of the record selected at Level 1 (the cursor is
/// the record index, frozen on drill — Up/Down is disabled at this level so
/// the view is stable, not a switcher). Shows the full thinking text, tool
/// input, and tool output (multi-line) rather than the one-line L1 summary.
pub(super) fn draw_event_detail(
    row: &TrajectoryRow,
    detail: &TrajectoryDetailView,
    cursor: usize,
    _area: Rect,
) -> (
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    usize,
) {
    let mut header = Vec::new();
    let mut body = Vec::new();
    let TrajectoryRow::Turn(_turn) = row else {
        return (header, body, vec![], 0);
    };
    match detail.state {
        TrajectoryDetailState::Loading => {
            body.push(line(vec![sp(
                "  reading this turn's records...",
                Color::DarkGray,
            )]));
            return (header, body, vec![key_hint(&[("Esc", "back")])], 0);
        }
        TrajectoryDetailState::Failed => {
            body.push(line(vec![sp(
                "  could not read this turn's records",
                Color::Red,
            )]));
            return (header, body, vec![key_hint(&[("Esc", "back")])], 0);
        }
        TrajectoryDetailState::Stale => return draw_drill_gone(),
        TrajectoryDetailState::Ready => {}
    }
    let ev = detail
        .records
        .get(cursor)
        .or_else(|| detail.records.first());
    let Some(ev) = ev else {
        return (header, body, vec![], 0);
    };
    let idx = cursor.min(detail.records.len().saturating_sub(1));
    let mark = ev.outcome.glyph();
    let mc = outcome_color(ev.outcome);
    let name = match (ev.ordinal, ev.name.as_deref()) {
        (0, Some(n)) => format!(" {n}"),
        (0, None) => String::new(),
        (ordinal, Some(n)) => format!(" {ordinal} {n}"),
        (ordinal, None) => format!(" {ordinal}"),
    };
    let mut ev_head = vec![
        sp(format!(" {}{} ", ev.kind.label(), name), Color::Cyan),
        sp(truncate_width(&ev.summary, 48), Color::White),
    ];
    // An unmeasured span is absent here too: a header that printed 0ms would
    // state a measurement the log does not have, however the body reads.
    if ev.duration_ms > 0 {
        ev_head.push(sp(
            format!("  {} ", format_span_ms(ev.duration_ms)),
            Color::Gray,
        ));
    }
    ev_head.push(sp(mark, mc));
    ev_head.push(sp(
        format!("  · Record {} of {}", idx + 1, detail.records.len()),
        Color::DarkGray,
    ));
    header.push(line(ev_head));
    header.push(blank());

    // Push a labeled field; multi-line strings split into one line per row so
    // the full content shows instead of being squashed onto one line.
    let push_field = |body: &mut Vec<Line<'static>>, label: &str, text: &str, color: Color| {
        let mut first = true;
        for line_text in text.split('\n') {
            let prefix = if first {
                format!(" {label}: ",)
            } else {
                "        ".to_string()
            };
            body.push(line(vec![
                sp(prefix, Color::DarkGray),
                sp(line_text.to_string(), color),
            ]));
            first = false;
        }
    };

    // L2 detail renders every field the record CARRIES, by presence — not a
    // kind-name table. A kind-name table would mask a projection that emits
    // fewer fields than the pane expects, so a real session drilled to L2
    // showed an empty body while the demonstration rows rendered fine.
    // Field-presence drives rendering for every kind, present and future, and
    // the header already labels the kind.
    // Redact secrets before rendering — the trajectory pane is a human-facing
    // surface (screen-share / recording / scrollback), and tool I/O + reasoning
    // can carry real secrets (an .env cat, a credentials read). The durable
    // log stays full-fidelity; only the display is filtered. See redaction.rs.
    // Each content field is its own block, so the boundary is not the reader's job.
    if let Some(thinking) = &ev.thinking {
        let r = crate::redaction::redact(thinking);
        push_field(&mut body, "thinking", &r, Color::Gray);
        body.push(blank());
    }
    if let Some(input) = &ev.input {
        let r = crate::redaction::redact(input);
        push_field(&mut body, "input", &r, Color::Gray);
        body.push(blank());
    }
    if let Some(output) = &ev.output {
        let r = crate::redaction::redact(output);
        push_field(&mut body, "output", &r, mc);
        body.push(blank());
    }
    push_model_facts(&mut body, ev, &push_field);
    body.push(blank());
    // The record's own span and where it started in the turn. An unmeasured
    // span is absent rather than zero: a latency of 0ms would claim a
    // measurement the log does not have.
    let mut tail = vec![sp(" ", Color::DarkGray)];
    if ev.duration_ms > 0 {
        tail.push(sp("latency: ", Color::DarkGray));
        tail.push(sp(format_span_ms(ev.duration_ms), Color::Gray));
        tail.push(sp("  · ", Color::DarkGray));
    }
    tail.push(sp("start: ", Color::DarkGray));
    tail.push(sp(format!("{}ms", ev.start_ms), Color::Gray));
    body.push(line(tail));
    let footer = vec![key_hint(&[("Esc", "back")])];
    (header, body, footer, idx)
}

// Helpers

/// The turn's cache line for the L1 header: how much of the turn's input came
/// from the prompt cache, shown with the share when the input is known.
fn format_turn_cache(turn: &TrajectoryTurn) -> String {
    match (turn.cache_read, turn.tokens_in) {
        (Some(c), Some(tin)) if tin > 0 => {
            format!(
                " · {} ({:.0}%) cache hit",
                fmt_k(c as usize),
                100.0 * c as f64 / tin as f64
            )
        }
        (Some(c), _) if c > 0 => format!(" · {} cache hit", fmt_k(c as usize)),
        _ => String::new(),
    }
}

/// The latency split, provider usage, and retry count of a model call or
/// delegated child. Each part is rendered only when the log recorded it: an
/// unmeasured value stays absent rather than appearing as a zero.
fn push_model_facts(
    body: &mut Vec<Line<'static>>,
    ev: &TrajectoryRecord,
    push_field: &impl Fn(&mut Vec<Line<'static>>, &str, &str, Color),
) {
    if let Some(timing) = &ev.timing {
        let mut parts = vec![format!("total {}ms", timing.total_ms)];
        if let Some(ttft) = timing.ttft_ms {
            parts.push(format!("TTFT {ttft}ms"));
        }
        if let Some(decode) = timing.decode_ms {
            parts.push(format!("decode {decode}ms"));
        }
        push_field(body, "timing", &parts.join(" · "), Color::Gray);
        // The rate the call decoded at, from its own output and decode span.
        if let (Some(decode_ms), Some(out)) =
            (timing.decode_ms, ev.usage.as_ref().and_then(|u| u.output))
            && decode_ms > 0
        {
            push_field(
                body,
                "speed",
                &format!("{:.1} tok/s", out as f64 / (decode_ms as f64 / 1000.0)),
                Color::Gray,
            );
        }
    }
    if let Some(usage) = &ev.usage {
        let parts: Vec<String> = [
            usage.input.map(|v| format!("input {v}")),
            usage.cache_read.map(|v| format!("cache read {v}")),
            usage.cache_write.map(|v| format!("cache write {v}")),
            usage.output.map(|v| format!("output {v}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !parts.is_empty() {
            push_field(body, "tokens", &parts.join(" · "), Color::Gray);
        }
        // Reasoning tokens are a component of the output above, not a count of
        // their own: the line says so instead of leaving the number bare.
        if let Some(reasoning) = usage.reasoning {
            push_field(
                body,
                "reasoning",
                &format!("{reasoning} tokens (part of output)"),
                Color::Gray,
            );
        }
    }
    if ev.retries > 0 {
        push_field(
            body,
            "retries",
            &format!("{} length recovery", ev.retries),
            Color::Gray,
        );
    }
}
