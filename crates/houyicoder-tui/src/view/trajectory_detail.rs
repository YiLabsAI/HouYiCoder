//! Level 1 and Level 2 drill-down renderers for the trajectory pane.
//!
//! Level 1 draws one turn's record timeline; Level 2 draws the full detail of
//! the selected record. Both share the pane's line helpers and view types.

use super::*;

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
    traj: &TrajectoryView,
    turn_idx: usize,
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
    let Some(row) = traj.rows.get(turn_idx) else {
        return (
            vec![line(vec![sp("no row data", Color::DarkGray)])],
            vec![],
            vec![],
            0,
        );
    };
    let row = row.clone();
    match row {
        TrajectoryRow::Turn(turn) => {
            app.trajectory_at_bg.set(false);
            let clamped = cursor.min(turn.records.len().saturating_sub(1));
            let cache_str = format_turn_cache(&turn);
            header.push(line(vec![
                sp(
                    format!(
                        " T{}  \"{}\"",
                        turn.n,
                        truncate_width(&turn_title(&turn), 30)
                    ),
                    Color::Cyan,
                ),
                sp(
                    format!(
                        "  {}↓ {}↑{} · total {:.1}s",
                        fmt_k_opt(turn.tokens_in),
                        fmt_k_opt(turn.tokens_out),
                        cache_str,
                        turn.duration_ms as f64 / 1000.0
                    ),
                    Color::Gray,
                ),
            ]));
            header.push(blank());
            // Layout: prefix(2) + kind(7) + gap(1) + name(11) + bar(bar_area)
            // + gap(1) + dur(7) + gap(1) + summary.
            let bar_area = (area.width as usize).saturating_sub(62).max(8);
            let summary_w = (area.width as usize).saturating_sub(32 + bar_area).max(8);
            header.push(ruler_line(turn.duration_ms, bar_area));
            for (i, ev) in turn.records.iter().enumerate() {
                body.push(record_row(
                    ev,
                    i == clamped,
                    turn.duration_ms,
                    bar_area,
                    summary_w,
                ));
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
            app.trajectory_at_bg.set(true);
            header.push(line(vec![
                sp(format!(" [bg] {} ", bg.kind), Color::Cyan),
                sp(truncate_width(&bg.summary, 50), Color::White),
                sp(
                    format!("  {:.1}s", bg.duration_ms as f64 / 1000.0),
                    Color::Gray,
                ),
            ]));
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
            body.push(line(vec![
                sp(" latency: ", Color::DarkGray),
                sp(format!("{}ms", bg.duration_ms), Color::Gray),
            ]));
            let footer = vec![blank(), key_hint(&[("Esc", "back")])];
            (header, body, footer, 0)
        }
    }
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
    let summary = format!("{}{}", truncate_width(&ev.summary, summary_w), timing);
    line(vec![
        sp(prefix, Color::Cyan),
        sp(format!("{:7}", ev.kind.label()), Color::DarkGray),
        sp(" ", Color::DarkGray),
        sp(format!("{:<11}", truncate_width(&label, 11)), Color::Cyan),
        sp(bar, bc),
        sp(" ", Color::DarkGray),
        sp(
            format!("{:>5.1}s", ev.duration_ms as f64 / 1000.0),
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
    traj: &TrajectoryView,
    turn_idx: usize,
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
    let turn = traj.rows.get(turn_idx).and_then(|r| match r {
        TrajectoryRow::Turn(t) => Some(t),
        _ => None,
    });
    let Some(turn) = turn else {
        return (header, body, vec![], 0);
    };
    let ev = turn.records.get(cursor).or_else(|| turn.records.first());
    let Some(ev) = ev else {
        return (header, body, vec![], 0);
    };
    let idx = cursor.min(turn.records.len().saturating_sub(1));
    let mark = ev.outcome.glyph();
    let mc = outcome_color(ev.outcome);
    let name = match (ev.ordinal, ev.name.as_deref()) {
        (0, Some(n)) => format!(" {n}"),
        (0, None) => String::new(),
        (ordinal, Some(n)) => format!(" {ordinal} {n}"),
        (ordinal, None) => format!(" {ordinal}"),
    };
    header.push(line(vec![
        sp(format!(" {}{} ", ev.kind.label(), name), Color::Cyan),
        sp(truncate_width(&ev.summary, 48), Color::White),
        sp(
            format!("  {:.1}s ", ev.duration_ms as f64 / 1000.0),
            Color::Gray,
        ),
        sp(mark, mc),
        sp(
            format!("  · record {}/{}", idx + 1, turn.records.len()),
            Color::DarkGray,
        ),
    ]));
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
    if let Some(thinking) = &ev.thinking {
        let r = crate::redaction::redact(thinking);
        push_field(&mut body, "thinking", &r, Color::Gray);
    }
    if let Some(input) = &ev.input {
        let r = crate::redaction::redact(input);
        push_field(&mut body, "input", &r, Color::Gray);
    }
    if let Some(output) = &ev.output {
        let r = crate::redaction::redact(output);
        push_field(&mut body, "output", &r, mc);
    }
    push_model_facts(&mut body, ev, &push_field);
    body.push(blank());
    body.push(line(vec![
        sp(" latency: ", Color::DarkGray),
        sp(format!("{}ms", ev.duration_ms), Color::Gray),
        sp("  · start: ", Color::DarkGray),
        sp(format!("{}ms", ev.start_ms), Color::Gray),
    ]));
    let footer = vec![key_hint(&[("Esc", "back")])];
    (header, body, footer, idx)
}

// Helpers

/// The latency split, provider usage, and retry count of a model call. Each
/// part is rendered only when the log recorded it: an unmeasured value stays
/// absent rather than appearing as a zero.
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
    }
    if let Some(usage) = &ev.usage {
        let parts: Vec<String> = [
            usage.input.map(|v| format!("input {v}")),
            usage.cache_read.map(|v| format!("cache read {v}")),
            usage.cache_write.map(|v| format!("cache write {v}")),
            usage.output.map(|v| format!("output {v}")),
            usage.reasoning.map(|v| format!("reasoning {v}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !parts.is_empty() {
            push_field(body, "tokens", &parts.join(" · "), Color::Gray);
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
