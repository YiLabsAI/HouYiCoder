//! Level 1 of the trajectory pane: one turn's record timeline, and what the
//! pane knows about the turn it is showing.
//!
//! The Level 2 record body lives in the record_detail module; both share the
//! pane's line helpers and view types.

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
use crate::state::App;
use crate::view::relative_time::format_span_ms;

/// The colour a record's outcome is drawn in.
pub(super) fn outcome_color(outcome: RecordOutcome) -> Color {
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
            // What the turn's row could not carry: the thinking time and
            // tokens, the effort it asked for, and the chain of models it
            // used. A turn that switched models states the whole chain here,
            // so the rows below can name one model each without repeating it.
            let mut meta = Vec::new();
            match (turn.reasoning_ms, turn.reasoning_tokens) {
                (Some(ms), Some(tokens)) => meta.push(format!(
                    "thinking {} ({})",
                    format_span_ms(ms),
                    format_tokens(tokens as u64)
                )),
                (Some(ms), None) => meta.push(format!("thinking {}", format_span_ms(ms))),
                (None, Some(tokens)) => {
                    meta.push(format!("thinking {} tok", format_tokens(tokens as u64)))
                }
                (None, None) => {}
            }
            if !turn.efforts.is_empty() {
                meta.push(format!("effort {}", turn.efforts.join(", ")));
            }
            if !meta.is_empty() {
                for row in wrap_line(&format!("  {}", meta.join(" · ")), area.width as usize) {
                    header.push(line(vec![sp(row, Color::DarkGray)]));
                }
            }
            if turn.model_chain.len() > 1 {
                // A chain of several long ids is wider than the pane; wrapping
                // it keeps every model it names readable.
                for row in wrap_line(
                    &format!("  models: {}", turn.model_chain.join(" → ")),
                    area.width as usize,
                ) {
                    header.push(line(vec![sp(row, Color::DarkGray)]));
                }
            }
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
            // The axis the bars share: idle gaps collapse so a long wait does
            // not squeeze the work into one column. The ruler labels this same
            // axis, so a folded wait does not leave the scale saying one thing
            // and the bars another.
            let axis = ActiveAxis::new(&detail.records);
            if !compact {
                header.push(ruler_line(axis.total(), bar_area));
            }
            match detail.state {
                TrajectoryDetailState::Ready => {
                    for (i, ev) in detail.records.iter().enumerate() {
                        body.push(if compact {
                            compact_record_row(ev, i == clamped, area.width as usize)
                        } else {
                            record_row(ev, i == clamped, &axis, bar_area, summary_w)
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
    axis: &ActiveAxis,
    bar_area: usize,
    summary_w: usize,
) -> Line<'static> {
    let prefix = if selected { "▸ " } else { "  " };
    let bar = positioned_bar(ev.start_ms, ev.duration_ms, axis, bar_area);
    let bc = outcome_color(ev.outcome);
    // An instant is a fact, not a task: it has no span to draw and no outcome
    // to grade, so its row carries neither a duration nor a tick.
    let instantaneous = ev.duration_ms == 0
        && !matches!(
            ev.kind,
            TrajectoryRecordKind::Tool | TrajectoryRecordKind::Model | TrajectoryRecordKind::Agent
        );
    let mark = if instantaneous {
        " "
    } else {
        ev.outcome.glyph()
    };
    let kind = kind_cell(ev);
    let name = name_cell(ev);
    let summary = row_summary(ev, &name, summary_w);
    line(vec![
        sp(prefix, Color::Cyan),
        sp(pad(&kind, TIMELINE_KIND_W), Color::DarkGray),
        sp(" ", Color::DarkGray),
        // Padded by display columns, not characters: a wide glyph in the name
        // would otherwise shift the bar and everything after it.
        sp(
            pad(&truncate_width(&name, TIMELINE_NAME_W), TIMELINE_NAME_W),
            Color::Cyan,
        ),
        sp(bar, bc),
        sp(" ", Color::DarkGray),
        // Truncated to its column rather than padded: a span wider than the
        // column would push the row past the width it is drawn in.
        sp(
            format!(
                "{:>width$} ",
                if instantaneous {
                    String::new()
                } else {
                    truncate_width(&format_span_ms(ev.duration_ms), TIMELINE_DUR_W - 1)
                },
                width = TIMELINE_DUR_W - 1
            ),
            Color::Gray,
        ),
        sp(" ", Color::DarkGray),
        sp(summary, Color::White),
        sp(" ", Color::DarkGray),
        sp(mark, bc),
    ])
}

/// The kind column: the kind and, for a call, its place in the turn.
fn kind_cell(ev: &TrajectoryRecord) -> String {
    let text = if ev.ordinal > 0 {
        format!("{} {}", ev.kind.label(), ev.ordinal)
    } else {
        ev.kind.label().to_string()
    };
    truncate_width(&text, TIMELINE_KIND_W)
}

/// The name column: the model with its vendor prefix, or the tool's own name.
fn name_cell(ev: &TrajectoryRecord) -> String {
    match (ev.kind, ev.name.as_deref()) {
        (TrajectoryRecordKind::Model, Some(id)) => display_model_id(id),
        (_, Some(name)) => name.to_string(),
        _ => String::new(),
    }
}

/// The measured split of a model call, in the full and compact forms the
/// summary column may use. A part the log did not record stays absent.
fn timing_cells(ev: &TrajectoryRecord) -> (String, String) {
    let Some(t) = ev
        .timing
        .as_ref()
        .filter(|_| ev.kind == TrajectoryRecordKind::Model)
    else {
        return (String::new(), String::new());
    };
    let mut parts = Vec::new();
    let mut compact = Vec::new();
    if let Some(ttft) = t.ttft_ms {
        parts.push(format!("TTFT {}", format_span_ms(ttft)));
        compact.push(format!("TTFT {}", format_span_ms(ttft)));
    }
    if let (Some(decode_ms), Some(out)) = (t.decode_ms, ev.usage.as_ref().and_then(|u| u.output))
        && decode_ms > 0
    {
        let rate = format!("{:.1} tok/s", out as f64 / (decode_ms as f64 / 1000.0));
        parts.push(rate.clone());
        compact.push(rate);
    }
    // The split the provider's own blocks measured: how long the call spent
    // thinking and how long it spent producing output. A span the stream never
    // closed stays absent rather than reading as no time.
    if let Some(reasoning) = t.reasoning_ms {
        parts.push(format!("reasoning {}", format_span_ms(reasoning)));
    }
    if let Some(response) = t.response_ms {
        parts.push(format!("generating {}", format_span_ms(response)));
    }
    (parts.join(" · "), compact.join(" · "))
}

/// The summary column: the switch and the measured split lead, because the
/// tail is what a narrow column drops.
fn row_summary(ev: &TrajectoryRecord, name: &str, summary_w: usize) -> String {
    let (full, compact) = timing_cells(ev);
    // The suffix gives way in tiers: the full split, then the two figures a
    // reader scans for, then nothing. Clearing it outright hid
    // time-to-first-token on every call that also reported reasoning.
    let timing = if UnicodeWidthStr::width(full.as_str()) < summary_w {
        full
    } else if !compact.is_empty() && UnicodeWidthStr::width(compact.as_str()) < summary_w {
        compact
    } else {
        String::new()
    };
    let switch = ev
        .model_switch_from
        .as_ref()
        .map(|prior| format!("[switch: {} -> {}] ", display_model_id(prior), name))
        .unwrap_or_default();
    let lead = match (switch.is_empty(), timing.is_empty()) {
        (false, false) => format!("{switch}{timing} · "),
        (false, true) => switch,
        (true, false) => format!("{timing} · "),
        (true, true) => String::new(),
    };
    let room = summary_w.saturating_sub(UnicodeWidthStr::width(lead.as_str()));
    format!("{}{}", lead, truncate_width(&ev.summary, room))
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
