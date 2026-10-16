//! The Usage sub-tab: cumulative token and cache totals, the session's
//! latency summary, delegated work, and the per-model breakdown.
//!
//! A cache-read count is stated when the endpoints reported the field or when
//! it is above zero, and a share only over an account they fully reported: a
//! count is a floor on what was reused, while a quotient over a partial
//! account can read higher than the truth.

use unicode_width::UnicodeWidthStr;

use crate::command::render::{field, format_tokens};
use crate::view::relative_time::span_label;

use super::*;

/// The Usage tab's latency rows, read from the same typed summary the
/// trajectory pane reads: one computation, two surfaces, so the two cannot
/// disagree about the session. A row is omitted when the session recorded no
/// sample for it — an unmeasured value is not a zero.
fn render_usage_latency(app: &App, f: &impl Fn(&str, &str) -> String, s: &mut String) {
    let Some(log) = app.sessions.persistence.trajectory_log.as_ref() else {
        return;
    };
    let view = log.trajectory();
    let timing = view.timing;
    if timing.model_ms > 0 || timing.tool_ms > 0 {
        s.push_str(&f(
            "model / tool time",
            &format!(
                "{} / {}",
                span_label(timing.model_ms),
                span_label(timing.tool_ms)
            ),
        ));
    }
    if let (Some(avg), Some(p95), Some(p99)) =
        (timing.ttft_avg_ms, timing.ttft_p95_ms, timing.ttft_p99_ms)
    {
        s.push_str(&f(
            "ttft",
            &format!(
                "{} avg · {} p95 · {} p99 ({} samples)",
                span_label(avg),
                span_label(p95),
                span_label(p99),
                timing.ttft_samples
            ),
        ));
    }
    if let Some(tps) = timing.decode_tok_per_sec {
        s.push_str(&f(
            "decode speed",
            &format!("{tps:.1} tok/s ({} samples)", timing.decode_samples),
        ));
    }
    // Delegated work is already inside the token rows above: the runner folds a
    // child's usage into its cumulative tally. This row exists to say how much
    // of that total the subagents contributed, so the reader can see the split.
    if let Some(subagent) = view.subagent_usage {
        // A child that returned before reporting usage leaves every field at
        // zero; that is an unmeasured cost, not a free one, so the row says so
        // instead of printing zeroes.
        let mut parts: Vec<String> = Vec::new();
        // A count is named when the children reported the field or when it is
        // above zero, so a field no child measured leaves its zero out of the
        // row instead of stating a cost the children never reported. The
        // output total carries no report flag on the durable record, so a zero
        // there cannot be told from an omitted count: it stays off the row,
        // the same floor rule the call rows use.
        if count_visible(subagent.input, subagent.provenance.input_reported) {
            parts.push(format!("{} input", format_tokens(subagent.input)));
        }
        if subagent.output > 0 {
            parts.push(format!("{} output", format_tokens(subagent.output)));
        }
        // The count is named whenever it is above zero, and the share joins it
        // only over an account the children fully reported: a count with no
        // complete account behind it keeps the count and drops the share,
        // while a measured miss states the share alone. The same two levels
        // the turn header uses.
        match (subagent.cache_hit_pct(), subagent.cache_read) {
            (Some(pct), cache_read) if cache_read > 0 => {
                parts.push(format!("{} ({pct:.0}% cached)", format_tokens(cache_read)))
            }
            (Some(_), _) => parts.push("0% cached".to_string()),
            (None, cache_read) if cache_read > 0 => {
                parts.push(format!("{} cached", format_tokens(cache_read)));
            }
            _ => {}
        }
        let detail = if parts.is_empty() {
            format!("usage not reported ({} subagent calls)", subagent.calls)
        } else {
            format!(
                "{} ({} subagent calls, included above)",
                parts.join(" · "),
                subagent.calls
            )
        };
        s.push_str(&f("delegated usage", &detail));
    }
}

pub(super) fn render_usage(app: &App) -> String {
    let f = field;
    let snap = app.snapshot_or_stub();
    let u = &snap.cumulative_usage;
    let ft = format_tokens;
    let mut s = String::new();
    s.push_str(&f("input tokens", &ft(u.input_tokens as u64)));
    s.push_str(&f("output tokens", &ft(u.output_tokens as u64)));
    // Reasoning tokens: a component of output, shown only when >0. The
    // parenthetical note "(incl. in output)" makes the inclusion relation
    // explicit so it is not read as a separate total (I14).
    if u.reasoning_tokens > 0 {
        s.push_str(&f(
            "reasoning",
            &format!("{} (incl. in output)", ft(u.reasoning_tokens as u64)),
        ));
    }
    let cache_read = u.cache_read_input_tokens as u64;
    if count_visible(cache_read, u.provenance.cache_read_reported) {
        let cache_value = match u.cache_hit_pct() {
            Some(pct) => format!("{} ({pct:.1}% of input)", ft(cache_read)),
            None => ft(cache_read),
        };
        s.push_str(&f("cached input", &cache_value));
    }
    // Not every provider reports cache creation. A zero cannot distinguish
    // an actual zero from an omitted field, so show this row only with data.
    if u.cache_write_input_tokens > 0 {
        s.push_str(&f("cache creation", &ft(u.cache_write_input_tokens as u64)));
    }
    s.push_str(&f(
        "tool calls",
        &format!(
            "{} ({} ok / {} err)",
            snap.tool_calls, snap.tool_success, snap.tool_errors
        ),
    ));
    // Session latency, from the same typed summary the trajectory pane reads.
    render_usage_latency(app, &f, &mut s);
    // Per-model breakdown only when two or more models share the session;
    // a single model is already covered by the flat rows above, so a
    // per-model section would just repeat them. Sorted by input+output
    // descending (heaviest first), the model-entries
    // ordering. Reasoning per model only when that model used any.
    if snap.by_model.len() >= 2 {
        s.push_str("Usage by model:\n");
        // The label column tracks the longest id so a long model name never
        // eats the separating space; 16 keeps short ids aligned. The extra
        // column beyond the colon guarantees at least one space of gap.
        let label_width = snap
            .by_model
            .iter()
            .map(|m| m.model.width() + 2)
            .max()
            .unwrap_or(0)
            .max(16);
        for m in &snap.by_model {
            let label = format!("{}:", m.model);
            let gap = " ".repeat(label_width.saturating_sub(label.width()));
            let mut row = format!(
                "  {label}{gap}{} input · {} output",
                ft(m.input_tokens),
                ft(m.output_tokens),
            );
            if m.reasoning_tokens > 0 {
                row.push_str(&format!(" · {} reasoning", ft(m.reasoning_tokens)));
            }
            let cache_pct = match m
                .provenance
                .cache_hit_pct(m.cache_read_tokens, m.input_tokens)
            {
                Some(pct) => format!(" ({pct:.1}%)"),
                None => String::new(),
            };
            if count_visible(m.cache_read_tokens, m.provenance.cache_read_reported) {
                row.push_str(&format!(" · {} cached{cache_pct}", ft(m.cache_read_tokens)));
            }
            if m.cache_write_tokens > 0 {
                row.push_str(&format!(" · {} cache creation", ft(m.cache_write_tokens)));
            }
            s.push_str(&row);
            s.push('\n');
        }
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests;
