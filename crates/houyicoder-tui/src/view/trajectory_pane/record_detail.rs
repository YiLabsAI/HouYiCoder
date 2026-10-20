//! Level 2 of the trajectory pane: the whole body of one record.
//!
//! The record is chosen a level up, so the keys here move the view over one
//! record's body rather than a cursor. Everything a Level 1 row could only
//! preview — the full model id, the measured split, the thinking, the tool
//! input and output — is read here.

use super::detail::{draw_drill_gone, outcome_color};
use super::*;
use crate::redaction::redact;

/// Level 2: the full detail of the record selected at Level 1. The record is
/// chosen a level up, so the keys here move the view over one record's whole
/// body rather than a cursor: the full thinking text, tool input, and tool
/// output, wrapped to the pane instead of cut off at its right edge.
pub(super) fn draw_event_detail(
    _turn: &TrajectoryTurn,
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
    let idx = cursor.min(detail.records.len().saturating_sub(1));
    let ev = detail.records.get(idx).or_else(|| detail.records.first());
    let Some(ev) = ev else {
        return (header, body, vec![], window);
    };
    let mark = ev.outcome.glyph();
    let mc = outcome_color(ev.outcome);
    // The detail is where the raw identity is checked, so the model is named
    // in full here even when the rows above had to shorten it.
    let name = if ev.kind == TrajectoryRecordKind::Model {
        format!(" {}", ev.name.as_deref().unwrap_or(""))
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
    let (input_label, output_label) = if ev.kind == TrajectoryRecordKind::Memory {
        ("injected", "recalled keys")
    } else {
        ("input", "output")
    };
    if let Some(prior) = &ev.model_switch_from {
        push_field(&mut body, "switched from", prior, Color::Gray);
        body.push(blank());
    }
    if let Some(input) = &ev.input {
        push_field(&mut body, input_label, &redact(input), Color::Gray);
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
            output_label,
            &redact(output),
            outcome_color(ev.outcome),
        );
        body.push(blank());
    }
    body
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
                .map(|v| match usage.input.filter(|input| *input > 0) {
                    // The share is what a reader compares across calls; the
                    // raw count alone does not say whether the prefix held.
                    Some(input) => {
                        format!("cache read {v} ({:.1}%)", (v as f64 / input as f64) * 100.0)
                    }
                    None => format!("cache read {v}"),
                }),
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
