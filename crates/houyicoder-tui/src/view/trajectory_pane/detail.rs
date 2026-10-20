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
    /// The turn's own facts, folded with its records. The row the drill came
    /// from is only the window's copy of them: once the records are in hand the
    /// drill renders from here, so a window that moves past the turn does not
    /// take the detail with it.
    pub turn: Option<TrajectoryTurn>,
    /// The turn's records in log order.
    pub records: Vec<TrajectoryRecord>,
    /// True when the turn was wider than one detail read, so the records shown
    /// are its beginning. A silent truncation would present part of a turn as
    /// the whole of it.
    pub truncated: bool,
}
use crate::redaction::redact;
use crate::state::App;
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
    turn: &TrajectoryTurn,
    detail: &TrajectoryDetailView,
    cursor: usize,
    area: Rect,
    app: &App,
) -> (
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    BodyScroll,
) {
    let mut header = Vec::new();
    let mut body = Vec::new();
    {
        {
            app.ui.trajectory.set_at_bg(false);
            let clamped = cursor.min(detail.records.len().saturating_sub(1));
            let cache_str = format_turn_cache(turn);
            header.push(line(vec![
                sp(
                    format!(" T{}  \"{}\"", turn.n, truncate_width(&turn.title, 30)),
                    Color::Cyan,
                ),
                sp(
                    format!(
                        "  {} in {} out{} · total {}",
                        tokens_or_dash(turn.tokens_in),
                        tokens_or_dash(turn.tokens_out),
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
                .clamp(8, TIMELINE_BAR_MAX_W);
            // No floor: on a narrow terminal the summary is what gives way, so
            // the row still ends with the duration and the outcome glyph.
            let summary_w = (area.width as usize)
                .saturating_sub(TIMELINE_PREFIX_W + bar_area + TIMELINE_SUFFIX_W);
            // A terminal too narrow to place a bar gets the row without it: a
            // bar drawn into columns the terminal does not have is a bar the
            // reader cannot use.
            let compact = (area.width as usize) < TIMELINE_MIN_W;
            if !compact {
                header.push(ruler_line(turn.duration_ms, bar_area));
            }
            match detail.state {
                TrajectoryDetailState::Ready => {
                    for (i, ev) in detail.records.iter().enumerate() {
                        body.push(if compact {
                            compact_record_row(ev, i == clamped, area.width as usize)
                        } else {
                            record_row(
                                ev,
                                i == clamped,
                                turn.duration_ms,
                                bar_area,
                                summary_w,
                                &turn.models,
                            )
                        });
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
            (header, body, footer, BodyScroll::Cursor(clamped))
        }
    }
}

/// A [bg] row drilled from L0 has no event timeline: show its detail directly
/// at L1 and flag it so Enter does not drill to L2.
pub(super) fn draw_bg_detail(
    bg: &TrajectoryBg,
    _cursor: usize,
    _area: Rect,
    app: &App,
) -> (
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    BodyScroll,
) {
    let mut header = Vec::new();
    let mut body = Vec::new();
    {
        {
            app.ui.trajectory.set_at_bg(true);
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
            (header, body, footer, BodyScroll::Cursor(0))
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
    BodyScroll,
) {
    (
        vec![blank()],
        vec![line(vec![sp(
            "  the turn this detail was about is no longer loaded",
            Color::DarkGray,
        )])],
        vec![blank(), key_hint(&[("Esc", "back")])],
        BodyScroll::Cursor(0),
    )
}

/// One Level 1 timeline row: the kind and name of the record, its bar on the
/// turn's time axis, its duration, and its summary with any measured latency
/// appended.
/// A record row without the time axis, for a terminal too narrow to place one.
///
/// The selection, the duration, and the outcome stay: they are what a row is
/// read for. The summary takes what is left, then the name gives way, and the
/// kind last.
fn compact_record_row(ev: &TrajectoryRecord, selected: bool, width: usize) -> Line<'static> {
    let bc = outcome_color(ev.outcome);
    let mark = ev.outcome.glyph();
    let duration = format_span_ms(ev.duration_ms);
    // The tail is what the row is read for, so it is the last thing to give
    // way: the duration goes before the outcome, and the selection before that.
    // A row is never wider than the terminal it is drawn in, however narrow.
    let tail = if width >= 4 + UnicodeWidthStr::width(duration.as_str()) + 2 {
        format!(" {duration} {mark}")
    } else if width >= 2 {
        format!(" {mark}")
    } else if width >= 1 {
        mark.to_string()
    } else {
        String::new()
    };
    let prefix = if UnicodeWidthStr::width(tail.as_str()) + 2 <= width {
        if selected { "▸ " } else { "  " }
    } else {
        ""
    };
    let mut left = width
        .saturating_sub(UnicodeWidthStr::width(prefix) + UnicodeWidthStr::width(tail.as_str()));
    let kind = ev.kind.label();
    let kind = if left > UnicodeWidthStr::width(kind) {
        left -= UnicodeWidthStr::width(kind) + 1;
        format!("{kind} ")
    } else {
        String::new()
    };
    // A name field carries its own separator, so it takes one column more than
    // the text it holds.
    let name = ev.name.as_deref().unwrap_or("");
    let name = if left >= 5 {
        let taken = left.min(TIMELINE_NAME_W + 1);
        left -= taken;
        format!("{} ", pad(&truncate_width(name, taken - 1), taken - 1))
    } else {
        String::new()
    };
    line(vec![
        sp(prefix, Color::Cyan),
        sp(kind, Color::DarkGray),
        sp(name, Color::Cyan),
        sp(truncate_width(&ev.summary, left), Color::White),
        sp(tail, bc),
    ])
}

fn record_row(
    ev: &TrajectoryRecord,
    selected: bool,
    turn_ms: u64,
    bar_area: usize,
    summary_w: usize,
    turn_models: &[String],
) -> Line<'static> {
    let prefix = if selected { "▸ " } else { "  " };
    let bar = positioned_bar(ev.start_ms, ev.duration_ms, turn_ms, bar_area);
    let bc = outcome_color(ev.outcome);
    let mark = ev.outcome.glyph();
    // The name column carries what the record acted on: the tool it called,
    // the agent it delegated to, the model it asked. A model id is shortened
    // against the turn's own set so two models stay distinguishable; the call
    // ordinal lives in the record detail, not in this column.
    let name = ev.name.as_deref().unwrap_or("");
    let label = if ev.kind == TrajectoryRecordKind::Model {
        short_model_name(name, turn_models)
    } else {
        name.to_string()
    };
    // A model call states its own measured split: the wait for the first token
    // and the decode rate of the tokens it produced. A part the log did not
    // record stays absent.
    let mut compact_timing = String::new();
    let timing = match &ev.timing {
        Some(t) if ev.kind == TrajectoryRecordKind::Model => {
            let mut parts = Vec::new();
            if let Some(ttft) = t.ttft_ms {
                parts.push(format!("TTFT {}", format_span_ms(ttft)));
                compact_timing = format!(" · TTFT {}", format_span_ms(ttft));
            }
            if let (Some(decode_ms), Some(out)) =
                (t.decode_ms, ev.usage.as_ref().and_then(|u| u.output))
                && decode_ms > 0
            {
                let rate = format!("{:.1} tok/s", out as f64 / (decode_ms as f64 / 1000.0));
                parts.push(rate.clone());
                compact_timing.push_str(&format!(" · {rate}"));
            }
            // The split the provider's own blocks measured: how long the call
            // spent thinking and how long it spent producing output. A span the
            // stream never closed stays absent rather than reading as no time.
            if let Some(reasoning) = t.reasoning_ms {
                parts.push(format!("reasoning {}", format_span_ms(reasoning)));
            }
            if let Some(response) = t.response_ms {
                parts.push(format!("generating {}", format_span_ms(response)));
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
    // The measured suffix gives way in tiers: the full split, then the two
    // figures a reader scans for, then the wait alone. Clearing it outright
    // hid time-to-first-token on every call that also reported reasoning.
    let timing = if UnicodeWidthStr::width(timing.as_str()) < summary_w {
        timing
    } else if !compact_timing.is_empty()
        && UnicodeWidthStr::width(compact_timing.as_str()) < summary_w
    {
        compact_timing
    } else {
        String::new()
    };
    let summary_w = summary_w.saturating_sub(UnicodeWidthStr::width(timing.as_str()));
    let summary_text = match &ev.model_switch_from {
        Some(prior) => format!("[model: {prior} -> {label}] {}", ev.summary),
        None => ev.summary.clone(),
    };
    let summary = format!("{}{}", truncate_width(&summary_text, summary_w), timing);
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
        // Truncated to its column rather than padded: a span wider than the
        // column would push the row past the width it is drawn in.
        sp(
            format!(
                "{:>width$} ",
                truncate_width(&format_span_ms(ev.duration_ms), TIMELINE_DUR_W - 1),
                width = TIMELINE_DUR_W - 1
            ),
            Color::Gray,
        ),
        sp(" ", Color::DarkGray),
        sp(summary, Color::White),
        sp(format!(" {}", mark), bc),
    ])
}

/// Level 2: the full detail of the record selected at Level 1. The record is
/// chosen a level up, so the keys here move the view over one record's whole
/// body rather than a cursor: the full thinking text, tool input, and tool
/// output, wrapped to the pane instead of cut off at its right edge.
pub(super) fn draw_event_detail(
    turn: &TrajectoryTurn,
    detail: &TrajectoryDetailView,
    cursor: usize,
    area: Rect,
    offset: usize,
) -> (
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    Vec<Line<'static>>,
    BodyScroll,
) {
    // The body belongs to the reader, so it is drawn from the offset the pane
    // holds: a redraw that arrives while they read must not take the view back
    // to the top.
    let window = BodyScroll::Offset(offset);
    let mut header = Vec::new();
    let mut body = Vec::new();
    match detail.state {
        TrajectoryDetailState::Loading => {
            body.push(line(vec![sp(
                "  reading this turn's records...",
                Color::DarkGray,
            )]));
            return (
                header,
                body,
                vec![blank(), key_hint(&[("Esc", "back")])],
                window,
            );
        }
        TrajectoryDetailState::Failed => {
            body.push(line(vec![sp(
                "  could not read this turn's records",
                Color::Red,
            )]));
            return (
                header,
                body,
                vec![blank(), key_hint(&[("Esc", "back")])],
                window,
            );
        }
        TrajectoryDetailState::Stale => return draw_drill_gone(),
        TrajectoryDetailState::Ready => {}
    }
    let ev = detail
        .records
        .get(cursor)
        .or_else(|| detail.records.first());
    let Some(ev) = ev else {
        return (header, body, vec![], window);
    };
    let idx = cursor.min(detail.records.len().saturating_sub(1));
    let mark = ev.outcome.glyph();
    let mc = outcome_color(ev.outcome);
    let name = if ev.kind == TrajectoryRecordKind::Model {
        let model = ev.name.as_deref().unwrap_or("");
        format!(" {}", short_model_name(model, &turn.models))
    } else {
        ev.name
            .as_deref()
            .map(|n| format!(" {n}"))
            .unwrap_or_default()
    };
    // The model-call ordinal belongs to the record's own identity, so it is
    // stated here rather than competing with the row's reading order.
    let model_calls: Vec<usize> = detail
        .records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == TrajectoryRecordKind::Model)
        .map(|(i, _)| i)
        .collect();
    let call_of = model_calls.iter().position(|i| *i == idx).map(|p| p + 1);
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
    if let Some(n) = call_of {
        // A detail read can stop at a byte budget, so the loaded records may be
        // the turn's beginning. The total is then a floor, not the count.
        let total = if detail.truncated {
            format!("{}+", model_calls.len())
        } else {
            model_calls.len().to_string()
        };
        ev_head.push(sp(format!(" · model call {n} of {total}"), Color::DarkGray));
    }
    header.push(line(ev_head));
    header.push(blank());
    let body = event_body(ev, area.width);
    let footer = vec![blank(), key_hint(&[("Up/Down", "scroll"), ("Esc", "back")])];
    (header, body, footer, window)
}

/// The body of one record: what it acted on, what it measured, and the prose it
/// produced, each as its own block.
///
/// Every field the record carries is rendered by presence rather than through a
/// table naming the fields of each record type: such a table would hide a fold
/// that emits fewer fields than the pane reads, leaving a record that could be
/// drilled into showing an empty body. Secrets are redacted here because the
/// pane is read by a person — over a screen share, in a recording, in
/// scrollback — while the log keeps its full fidelity.
///
/// What the record acted on comes first, then its measurements, then the long
/// prose: a hundred-line body ahead of the latency and the usage would push
/// them off the pane, and the reader would scroll past the text to reach the
/// numbers the text is read for.
fn event_body(ev: &TrajectoryRecord, width: u16) -> Vec<Line<'static>> {
    let mut body = Vec::new();
    // Wrap a field to the pane so a logical line wider than it is read in full
    // instead of being cut off at the right edge, and indent the rows it wraps
    // into under the value column, so a second row does not read as another
    // field.
    let push_field = |body: &mut Vec<Line<'static>>, label: &str, text: &str, color: Color| {
        let prefix = format!(" {label}: ");
        let indent = " ".repeat(UnicodeWidthStr::width(prefix.as_str()));
        let avail = (width as usize).saturating_sub(indent.len());
        let mut first = true;
        for logical in text.split('\n') {
            for row in wrap_line(logical, avail) {
                let lead = if first {
                    prefix.clone()
                } else {
                    indent.clone()
                };
                body.push(line(vec![sp(lead, Color::DarkGray), sp(row, color)]));
                first = false;
            }
        }
    };
    if let Some(input) = &ev.input {
        push_field(&mut body, "input", &redact(input), Color::Gray);
        body.push(blank());
    }
    let before_facts = body.len();
    push_model_facts(&mut body, ev, &push_field);
    push_record_span(&mut body, ev);
    // A record whose log carries no measurement is not left with a blank where
    // facts would have been.
    if body.len() > before_facts {
        body.push(blank());
    }
    if let Some(thinking) = &ev.thinking {
        push_field(&mut body, "thinking", &redact(thinking), Color::Gray);
        body.push(blank());
    }
    if let Some(output) = &ev.output {
        push_field(
            &mut body,
            "output",
            &redact(output),
            outcome_color(ev.outcome),
        );
        body.push(blank());
    }
    body
}

// Helpers

/// The turn's cache line for the L1 header: how much of the turn's input came
/// from the prompt cache. The share comes from the turn's own derivation, so the
/// header and the row above it cannot disagree about whether the account
/// supports one. A partial account keeps its raw count and loses only the
/// derived share.
fn format_turn_cache(turn: &TrajectoryTurn) -> String {
    // A count above zero is named before its share. A measured miss has no
    // count worth naming, so it is stated as the rate alone.
    match (turn.cache_hit_pct(), turn.cache_read) {
        (Some(pct), Some(cache_read)) if cache_read > 0 => {
            format!(" · {} ({pct:.0}%) cache hit", format_tokens(cache_read))
        }
        (Some(_), _) => " · 0% cache hit".to_string(),
        (None, Some(cache_read)) if cache_read > 0 => {
            format!(" · {} cache hit", format_tokens(cache_read))
        }
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
        // Every span here reads the way the session header and the Level 1 rows
        // read one: the same span shown twice must not read as two numbers.
        let mut parts = vec![format!("total {}", format_span_ms(timing.total_ms))];
        if let Some(ttft) = timing.ttft_ms {
            parts.push(format!("TTFT {}", format_span_ms(ttft)));
        }
        if let Some(decode) = timing.decode_ms {
            parts.push(format!("decode {}", format_span_ms(decode)));
        }
        // The split the provider's own blocks measured. A span the stream never
        // closed stays absent rather than reading as no time.
        if let Some(reasoning) = timing.reasoning_ms {
            parts.push(format!("reasoning {}", format_span_ms(reasoning)));
        }
        if let Some(response) = timing.response_ms {
            parts.push(format!("generating {}", format_span_ms(response)));
        }
        push_field(body, "timing", &parts.join(" · "), Color::Gray);
        // The rate the call decoded at, from its own output and decode span.
        if let (Some(decode_ms), Some(out)) =
            (timing.decode_ms, ev.usage.as_ref().and_then(|u| u.output))
            && decode_ms > 0
            && out > 0
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
        // A breakdown count the call's endpoint did not report and the log left
        // at zero is unknown rather than a measured zero, so it stays off the
        // line.
        let parts: Vec<String> = [
            usage
                .input
                .filter(|v| count_visible(*v, usage.provenance.input_reported))
                .map(|v| format!("input {v}")),
            usage
                .cache_read
                .filter(|v| count_visible(*v, usage.provenance.cache_read_reported))
                .map(|v| format!("cache read {v}")),
            usage
                .cache_write
                .filter(|v| count_visible(*v, usage.provenance.cache_write_reported))
                .map(|v| format!("cache write {v}")),
            // The output total carries no report flag on the durable record, so
            // a zero cannot be told from an omitted count: it stays off the
            // line, while a count above zero stands on its own as a floor.
            usage
                .output
                .filter(|v| *v > 0)
                .map(|v| format!("output {v}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !parts.is_empty() {
            push_field(body, "tokens", &parts.join(" · "), Color::Gray);
        }
        // Reasoning tokens are a component of the output above, not a count of
        // their own: the line says so instead of leaving the number bare. Same
        // missing flag as the output total, so the same floor rule applies.
        if let Some(reasoning) = usage.reasoning.filter(|v| *v > 0) {
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

/// The record's own span and where it started in the turn, read the way the
/// rest of the pane reads a span. This line belongs with the measurements
/// above the prose for the same reason they do: below a long body it is off
/// the pane the moment the reader needs it. An unmeasured value stays absent
/// rather than reading as a measured zero: a start of 0ms, like a latency of
/// 0ms, would state a measurement the log does not have.
fn push_record_span(body: &mut Vec<Line<'static>>, ev: &TrajectoryRecord) {
    let mut span = Vec::new();
    if ev.duration_ms > 0 {
        span.push(sp(" latency: ", Color::DarkGray));
        span.push(sp(format_span_ms(ev.duration_ms), Color::Gray));
    }
    if ev.start_ms > 0 {
        let lead = if span.is_empty() { " " } else { "  ·  " };
        span.push(sp(lead, Color::DarkGray));
        span.push(sp("start: ", Color::DarkGray));
        span.push(sp(format_span_ms(ev.start_ms), Color::Gray));
    }
    if !span.is_empty() {
        body.push(line(span));
    }
}
