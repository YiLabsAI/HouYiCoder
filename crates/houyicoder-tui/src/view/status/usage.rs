//! The Usage sub-tab: cumulative token and cache totals, the session's
//! latency summary, delegated work, and the per-model breakdown.

use super::*;

/// The Usage tab's latency rows, read from the same typed summary the
/// trajectory pane reads: one computation, two surfaces, so the two cannot
/// disagree about the session. A row is omitted when the session recorded no
/// sample for it — an unmeasured value is not a zero.
fn render_usage_latency(app: &App, f: &impl Fn(&str, &str) -> String, s: &mut String) {
    let Some(log) = app.trajectory_log.as_ref() else {
        return;
    };
    let view = log.trajectory();
    let timing = view.timing;
    if timing.model_ms > 0 || timing.tool_ms > 0 {
        s.push_str(&f(
            "model / tool time",
            &format!(
                "{:.1}s / {:.1}s",
                timing.model_ms as f64 / 1000.0,
                timing.tool_ms as f64 / 1000.0
            ),
        ));
    }
    if let (Some(avg), Some(p95), Some(p99)) =
        (timing.ttft_avg_ms, timing.ttft_p95_ms, timing.ttft_p99_ms)
    {
        s.push_str(&f(
            "ttft",
            &format!(
                "{:.1}s avg · {:.1}s p95 · {:.1}s p99 ({} samples)",
                avg as f64 / 1000.0,
                p95 as f64 / 1000.0,
                p99 as f64 / 1000.0,
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
    // Delegated work is reported separately rather than folded into the rows
    // above: those come from the parent's own provider calls, and adding the
    // children would make this number disagree with the runner's cumulative
    // usage. The row says so, so the reader knows how to combine them.
    if let Some(delegated) = view.subagent_usage {
        // A child that returned before reporting usage leaves every field at
        // zero; that is an unmeasured cost, not a free one, so the row says so
        // instead of printing zeroes.
        let detail = if delegated.input == 0 && delegated.output == 0 {
            format!("usage not reported ({} subagent calls)", delegated.calls)
        } else {
            let cache = match delegated.cache_hit_pct() {
                Some(pct) => format!(" · {pct:.0}% cached"),
                None => String::new(),
            };
            format!(
                "{} input · {} output{cache} ({} subagent calls, not in the rows above)",
                format_tokens(delegated.input),
                format_tokens(delegated.output),
                delegated.calls
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
    let cache_value = if u.input_tokens > 0 {
        format!(
            "{} ({:.1}% of input)",
            ft(cache_read),
            100.0 * cache_read as f64 / u.input_tokens as f64
        )
    } else {
        ft(cache_read)
    };
    s.push_str(&f("cached input", &cache_value));
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
            let cache_pct = if m.input_tokens > 0 {
                format!(
                    " ({:.1}%)",
                    100.0 * m.cache_read_tokens as f64 / m.input_tokens as f64
                )
            } else {
                String::new()
            };
            row.push_str(&format!(
                " · {} cached{}",
                ft(m.cache_read_tokens),
                cache_pct
            ));
            if m.cache_write_tokens > 0 {
                row.push_str(&format!(" · {} cache creation", ft(m.cache_write_tokens)));
            }
            s.push_str(&row);
            s.push('\n');
        }
    }
    s.trim_end().to_string()
}
