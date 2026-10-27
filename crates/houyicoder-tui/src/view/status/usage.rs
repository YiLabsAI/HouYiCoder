//! The Usage sub-tab: cumulative token and cache totals, the session's
//! latency summary, delegated work, and the per-model breakdown.
//!
//! A cache-read count is stated when the endpoints reported the field or when
//! it is above zero, and a share only over an account they fully reported: a
//! count is a floor on what was reused, while a quotient over a partial
//! account can read higher than the truth.

use houyicoder_protocol::llm::UsageProvenance;
use unicode_width::UnicodeWidthStr;

use crate::command::render::{field, format_tokens};
use crate::view::relative_time::span_label;
use crate::view::trajectory_pane::TrajectoryView;

use super::*;

/// The Usage tab's latency rows, read from the same typed summary the
/// trajectory pane reads: one computation, two views, so the two cannot
/// disagree about the session. A row is omitted when the session recorded no
/// sample for it — an unmeasured value is not a zero.
fn render_usage_latency(
    view: Option<&TrajectoryView>,
    f: &impl Fn(&str, &str) -> String,
    s: &mut String,
) {
    let Some(view) = view else {
        return;
    };
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
}

/// The rows splitting the totals above between the work the user asked for and
/// the background calls the session made on its own.
///
/// Both halves are already inside the token totals: the status snapshot folds
/// a child's account and the recaps' spend into it. These rows exist so the
/// reader can see the split instead of subtracting for it.
/// They sit next to the totals rather than after the latency rows, because the
/// pane's body is shorter than its content on a common terminal and the split
/// of the token spend outranks the shape of the timings; the latency rows are
/// what should fall off the bottom first.
fn render_background_spend(
    view: Option<&TrajectoryView>,
    f: &impl Fn(&str, &str) -> String,
    s: &mut String,
) {
    let Some(view) = view else {
        return;
    };
    if let Some(subagent) = view.subagent_usage {
        let parts = spend_parts(
            subagent.input,
            subagent.output,
            subagent.cache_read,
            subagent.provenance,
        );
        let row = spend_row(&parts, "subagent call", subagent.calls, subagent.unmeasured);
        s.push_str(&f("delegated usage", &row));
    }
    if let Some(recap) = view.recap_usage {
        let mut parts = spend_parts(
            recap.input,
            recap.output,
            recap.cache_read,
            recap.provenance,
        );
        if let Some(ms) = recap.duration_ms {
            parts.push(span_label(ms));
        }
        let row = spend_row(&parts, "recap", recap.calls, recap.unmeasured);
        s.push_str(&f("recap usage", &row));
    }
}

/// The figures one background caller reported, under the floor rules the call
/// rows use.
///
/// A count is named when it was reported or when it is above zero, so a field
/// nobody measured leaves its zero out of the row instead of stating a cost
/// that was never reported. The output total carries no report flag on the
/// durable record, so a zero there cannot be told from an omitted count: it
/// stays off the row. The cache count is named whenever it is above zero, and
/// the share joins it only over an account that fully reported both counts.
fn spend_parts(
    input: u64,
    output: u64,
    cache_read: u64,
    provenance: UsageProvenance,
) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    if count_visible(input, provenance.input_reported) {
        parts.push(format!("{} input", format_tokens(input)));
    }
    if output > 0 {
        parts.push(format!("{} output", format_tokens(output)));
    }
    match (provenance.cache_hit_pct(cache_read, input), cache_read) {
        (Some(pct), cache_read) if cache_read > 0 => {
            parts.push(format!("{} ({pct:.0}% cached)", format_tokens(cache_read)))
        }
        (Some(_), _) => parts.push("0% cached".to_string()),
        (None, cache_read) if cache_read > 0 => {
            parts.push(format!("{} cached", format_tokens(cache_read)));
        }
        _ => {}
    }
    parts
}

/// One background caller's row text. No figures at all is an unmeasured cost,
/// not a free one, so the row says so instead of printing zeroes. A row whose
/// every call went unmeasured says the same even if a figure slipped through:
/// nothing it could print was measured, so nothing is inside the totals above.
///
/// The count leads because the pane does not wrap: on a terminal narrower than
/// the row the tail is clipped, and how many calls the figures cover is what a
/// reader needs before the figures themselves. When some calls wrote no figure
/// the sum is a floor, so the count says how many were measured rather than
/// letting the sum pass for the whole.
fn spend_row(parts: &[String], one: &str, calls: usize, unmeasured: usize) -> String {
    let counted = if calls == 1 {
        format!("1 {one}")
    } else {
        format!("{calls} {one}s")
    };
    if parts.is_empty() || unmeasured >= calls {
        return format!("usage not reported ({counted})");
    }
    let counted = if unmeasured == 0 {
        counted
    } else {
        let measured = calls.saturating_sub(unmeasured);
        format!("{measured} of {counted} measured")
    };
    format!("{counted} (included above) · {}", parts.join(" · "))
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
    // One read of the session's typed summary serves both the split and the
    // latency rows, so a frame pays for it once.
    let trajectory = app
        .sessions
        .persistence
        .trajectory_log
        .as_ref()
        .map(|log| log.trajectory());
    // The split of the totals above, before the latency detail.
    render_background_spend(trajectory.as_deref(), &f, &mut s);
    // Session latency, from the same typed summary the trajectory pane reads.
    render_usage_latency(trajectory.as_deref(), &f, &mut s);
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
