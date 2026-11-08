//! Context-usage suggestions derived from the measured breakdown: a window
//! nearing capacity, a tool whose call and result tokens crowd it, a file-read
//! habit, and memory files grown past their band. Each band is a share of the
//! window paired with a token floor, so a small window does not raise advice
//! about a harmless cost. Shared by the preview path and the real /context path
//! so the logic is not duplicated.

use crate::command::render::format_tokens;
use crate::records::{ContextSuggestion, SuggestionSeverity};
use crate::transcript::records::capitalize;
use houyicoder_protocol::frontend::context::{ContextBreakdown, ContextFileEntry};

/// The window fill at which the session starts folding turns on its own.
const NEAR_CAPACITY_PERCENT: f64 = 80.0;
/// The share of the window one tool must take before its own row appears.
const LARGE_TOOL_RESULT_PERCENT: f64 = 15.0;
/// The floor below which a tool row is noise whatever its share.
const LARGE_TOOL_RESULT_TOKENS: u32 = 10_000;
/// The share file reads must take to be worth naming on their own.
const READ_BLOAT_PERCENT: f64 = 5.0;
/// The band memory files pass before pruning is worth suggesting.
const MEMORY_HIGH_PERCENT: f64 = 5.0;
/// The floor below which memory files are not worth a row.
const MEMORY_HIGH_TOKENS: u32 = 5_000;

/// Context-usage suggestions derived from the measured breakdown: a window
/// nearing capacity, a tool whose call and result tokens crowd it, a file-read
/// habit, and memory files grown past their band.
pub fn suggestions_for(breakdown: &ContextBreakdown) -> Vec<ContextSuggestion> {
    let mut out: Vec<ContextSuggestion> = Vec::new();
    push_near_capacity(breakdown, &mut out);
    push_compact_bloat(breakdown, &mut out);
    push_tool_footprints(breakdown, &mut out);
    push_read_bloat(breakdown, &mut out);
    push_memory_bloat(breakdown, &mut out);
    // Warnings first, then the largest saving, so the row a reader can act on
    // most sits at the top of the section.
    out.sort_by(|a, b| {
        severity_rank(a.severity)
            .cmp(&severity_rank(b.severity))
            .then_with(|| {
                b.savings_tokens
                    .unwrap_or(0)
                    .cmp(&a.savings_tokens.unwrap_or(0))
            })
    });
    out
}

/// A token figure's share of the window, as a percentage. Zero when the window
/// is unknown, so a breakdown without one raises no share-based suggestion.
fn share_pct(tokens: u32, window: u32) -> f64 {
    if window == 0 {
        0.0
    } else {
        100.0 * f64::from(tokens) / f64::from(window)
    }
}

/// Warnings sort before informational rows.
fn severity_rank(severity: SuggestionSeverity) -> u8 {
    match severity {
        SuggestionSeverity::Warning => 0,
        SuggestionSeverity::Info => 1,
    }
}

/// A window nearing its cap: the point where the session folds older turns on
/// its own, so the user can choose what to keep instead of losing the choice.
fn push_near_capacity(breakdown: &ContextBreakdown, out: &mut Vec<ContextSuggestion>) {
    let pct = share_pct(breakdown.total_tokens, breakdown.context_window);
    if pct < NEAR_CAPACITY_PERCENT {
        return;
    }
    out.push(ContextSuggestion {
        severity: SuggestionSeverity::Warning,
        title: format!("Context is {pct:.0}% full"),
        detail: "Older turns fold automatically once the window fills. \
                 Use /compact now to choose what gets kept."
            .to_string(),
        savings_tokens: None,
    });
}

/// Folded history summary grown large enough that another compaction or a
/// fresh session would reclaim substantial window space.
fn push_compact_bloat(breakdown: &ContextBreakdown, out: &mut Vec<ContextSuggestion>) {
    let Some(compact) = breakdown
        .categories
        .iter()
        .find(|c| c.label == "Compact buffer" && c.tokens >= 30_000)
    else {
        return;
    };
    let pct = share_pct(compact.tokens, breakdown.context_window);
    let figure = format_tokens(u64::from(compact.tokens));
    out.push(ContextSuggestion {
        severity: SuggestionSeverity::Warning,
        title: format!("Compacted history using {figure} tokens ({pct:.0}%)"),
        detail: "Accumulated compaction summaries take a large share of the window. \
                 Run /compact to condense earlier summaries or /clear to start fresh."
            .to_string(),
        savings_tokens: Some((f64::from(compact.tokens) * 0.6) as u32),
    });
}

/// One row per tool whose call blocks plus results take a large share of the
/// window. A tool outside the table is reported only once it alone takes a
/// fifth, so an unlisted tool cannot fill the section with rows.
fn push_tool_footprints(breakdown: &ContextBreakdown, out: &mut Vec<ContextSuggestion>) {
    let used_context = breakdown.total_tokens.max(1);
    for footprint in &breakdown.tool_footprints {
        let tokens = footprint
            .call_tokens
            .saturating_add(footprint.result_tokens);
        let pct = share_pct(tokens, breakdown.context_window);
        let pct_of_used = 100.0 * f64::from(tokens) / f64::from(used_context);
        let crosses_threshold = (pct >= LARGE_TOOL_RESULT_PERCENT
            && tokens >= LARGE_TOOL_RESULT_TOKENS)
            || (breakdown.context_window > 200_000
                && pct_of_used >= 20.0
                && tokens >= LARGE_TOOL_RESULT_TOKENS)
            || tokens >= 30_000;
        if !crosses_threshold {
            continue;
        }
        if let Some(suggestion) = large_tool_suggestion(&footprint.tool, tokens, pct) {
            out.push(suggestion);
        }
    }
}

/// The advice for one tool's footprint: the severity, the tool's display name,
/// the way its results can be limited, and the share a reader could plausibly
/// reclaim. None for a tool the table does not name below the fifth-of-window
/// mark. Names match case-insensitively so a spelling change to a tool name
/// cannot leave the advice silently unfired.
fn large_tool_suggestion(tool: &str, tokens: u32, pct: f64) -> Option<ContextSuggestion> {
    let figure = format_tokens(u64::from(tokens));
    let lowered = tool.to_ascii_lowercase();
    let (severity, title, detail, reclaim) = match lowered.as_str() {
        "bash" => (
            SuggestionSeverity::Warning,
            format!("Bash results using {figure} tokens ({pct:.0}%)"),
            "Pipe output through head, tail, or grep to reduce result size. \
             Avoid reading a large file with a shell command: read the part \
             you need with an offset and a limit."
                .to_string(),
            0.5,
        ),
        "read" => (
            SuggestionSeverity::Info,
            format!("Read results using {figure} tokens ({pct:.0}%)"),
            "Use offset and limit to read only the sections you need. A file \
             already read can be referred to instead of read again."
                .to_string(),
            0.3,
        ),
        "grep" => (
            SuggestionSeverity::Info,
            format!("Grep results using {figure} tokens ({pct:.0}%)"),
            "Add more specific patterns, or use the file-type and glob \
             parameters to search fewer files. A file-name lookup is cheaper \
             than a content search."
                .to_string(),
            0.3,
        ),
        "webfetch" => (
            SuggestionSeverity::Info,
            format!("WebFetch results using {figure} tokens ({pct:.0}%)"),
            "A fetched page can be very large. Ask for the part of it you need.".to_string(),
            0.4,
        ),
        _ if pct >= 20.0 => (
            SuggestionSeverity::Info,
            format!("{} using {figure} tokens ({pct:.0}%)", capitalize(tool)),
            "This tool alone takes a fifth of the window or more.".to_string(),
            0.2,
        ),
        _ => return None,
    };
    Some(ContextSuggestion {
        severity,
        title,
        detail,
        savings_tokens: Some((f64::from(tokens) * reclaim) as u32),
    })
}

/// A file-read habit below the tool band: result tokens past the read share
/// that the tool row above would not already have named. The name is matched
/// the way the tool table matches its own, so a spelling change to the
/// declared name cannot leave this row silent.
fn push_read_bloat(breakdown: &ContextBreakdown, out: &mut Vec<ContextSuggestion>) {
    let Some(read) = breakdown
        .tool_footprints
        .iter()
        .find(|footprint| footprint.tool.eq_ignore_ascii_case("read"))
    else {
        return;
    };
    let total = read.call_tokens.saturating_add(read.result_tokens);
    let total_pct = share_pct(total, breakdown.context_window);
    let used_context = breakdown.total_tokens.max(1);
    let pct_of_used = 100.0 * f64::from(total) / f64::from(used_context);
    if (total_pct >= LARGE_TOOL_RESULT_PERCENT && total >= LARGE_TOOL_RESULT_TOKENS)
        || (breakdown.context_window > 200_000
            && pct_of_used >= 20.0
            && total >= LARGE_TOOL_RESULT_TOKENS)
        || total >= 30_000
    {
        return;
    }
    let result_pct = share_pct(read.result_tokens, breakdown.context_window);
    let result_pct_of_used = 100.0 * f64::from(read.result_tokens) / f64::from(used_context);
    let crosses_threshold = (result_pct >= READ_BLOAT_PERCENT
        && read.result_tokens >= LARGE_TOOL_RESULT_TOKENS)
        || (breakdown.context_window > 200_000
            && result_pct_of_used >= 10.0
            && read.result_tokens >= LARGE_TOOL_RESULT_TOKENS)
        || read.result_tokens >= 20_000;
    if !crosses_threshold {
        return;
    }
    out.push(ContextSuggestion {
        severity: SuggestionSeverity::Info,
        title: format!(
            "File reads using {} tokens ({result_pct:.0}%)",
            format_tokens(u64::from(read.result_tokens))
        ),
        detail: "Re-reading a file repeats its whole cost. A range read costs \
                 only the range, and referencing an earlier read costs nothing."
            .to_string(),
        savings_tokens: Some((f64::from(read.result_tokens) * 0.3) as u32),
    });
}

/// Memory files past their band, naming the three largest so the user knows
/// which file to prune rather than only that pruning would help.
fn push_memory_bloat(breakdown: &ContextBreakdown, out: &mut Vec<ContextSuggestion>) {
    let total = breakdown
        .memory_files
        .iter()
        .fold(0u32, |sum, file| sum.saturating_add(file.tokens));
    let pct = share_pct(total, breakdown.context_window);
    let crosses_threshold =
        (pct >= MEMORY_HIGH_PERCENT && total >= MEMORY_HIGH_TOKENS) || total >= 20_000;
    if !crosses_threshold {
        return;
    }
    let mut largest: Vec<&ContextFileEntry> = breakdown.memory_files.iter().collect();
    largest.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.label.cmp(&b.label)));
    let names: Vec<String> = largest
        .iter()
        .take(3)
        .map(|file| format!("{} ({})", file.label, format_tokens(u64::from(file.tokens))))
        .collect();
    out.push(ContextSuggestion {
        severity: SuggestionSeverity::Info,
        title: format!(
            "Memory files using {} tokens ({pct:.0}%)",
            format_tokens(u64::from(total))
        ),
        detail: format!(
            "Largest: {}. Use /memory to review and prune stale entries.",
            names.join(", ")
        ),
        savings_tokens: Some((f64::from(total) * 0.3) as u32),
    });
}
