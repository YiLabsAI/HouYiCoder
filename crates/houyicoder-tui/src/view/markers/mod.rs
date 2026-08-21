//! Pure styling helpers for the transcript's glyph-led rows and result
//! bodies, extracted from working.rs so the layout file stays under the size
//! gate. styled_row colors a call or continuation row by outcome;
//! result_body_rows lays out a multi-line result body with a gutter,
//! continuation indent, and collapse + Ctrl+O expand. For an Edit/MultiEdit
//! result body the renderer parses the unified diff into structured hunks
//! (consuming the @@ header as metadata, never rendering it) and lays out a
//! line-numbered green/red gutter with a dim "..." between hunks — a
//! structured-hunk + numbered-line + interspersed-gap shape.
//! Both result helpers are pure (no App, no Frame) so the layout is unit-
//! testable in isolation.

use crate::records::ToolOutcome;
use crate::selection::{
    TAG_DIFF_ADD, TAG_DIFF_BORDER, TAG_DIFF_CTX, TAG_DIFF_DEL, TAG_DIFF_HUNK, TAG_PLAIN,
};
use crate::view::word_diff::WordDiffPart;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

mod diff_plan;
use diff_plan::{DiffKind, PlanKind, PlanRow, plan_diff_body};

/// One planned display row: the selection tag, the row text (gutter + content
/// for stable width + copy), the per-row outcome (Some only on the summary),
/// the result call_id (Some only on the summary, for Ctrl+O), and the word-
/// level diff parts (Some on a paired remove+add below the change threshold).
pub(crate) type PlannedRow = (
    u8,
    String,
    Option<ToolOutcome>,
    Option<String>,
    Option<Vec<WordDiffPart>>,
);

/// Always collapse a tool result body to the head (COLLAPSE_SHOW lines)
/// plus a Ctrl+O hint — caps at 3 wrapped lines always (no threshold below
/// which the full body shows).
/// A 5-line find output still shows 3 + "… +2 lines" so the transcript
/// never buries the rest. Expanded per-result via Ctrl+O (keyed by
/// call_id, survives the transcript rebuild on each event batch).
const COLLAPSE_SHOW: usize = 3;

/// Pre-truncate a body's logical lines to a byte budget (a maxChars pre-cut
/// of COLLAPSE_SHOW * wrap_width * 4). Stops at the last line that fits within
/// the budget; if even the first line exceeds it, takes a char-prefix so the
/// summary still shows something. Returns the kept lines + whether any lines
/// were dropped (the caller estimates the remaining count from the original
/// length, not from the kept prefix). A char-prefix (not a byte slice) keeps
/// the string valid UTF-8.
fn pretruncate_lines(blines: &[&str], max_chars: usize) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    for l in blines {
        let need = used + l.len() + 1;
        if need > max_chars {
            truncated = true;
            break;
        }
        used = need;
        out.push(l.to_string());
    }
    if out.is_empty() && !blines.is_empty() {
        // First line alone exceeds the budget — take a char-prefix; the line
        // was truncated (content cut) even though out has one element.
        out.push(blines[0].chars().take(max_chars).collect());
        truncated = true;
    }
    (out, truncated)
}

/// Style a glyph-led row (⏺ call or ⎿ continuation/result) with outcome-based
/// colors: the call glyph ⏺ is colored by the call's resolved outcome (running
/// cyan, success green, error red) + the tool name BOLD; the result gutter ⎿ is
/// dim with the body in the default foreground for success (stdout is just
/// text, not a semantic signal — coloring it green drowns out the one channel
/// reserved for errors) and red only for errors. An Edit diff summary bolds
/// the counts. Other rows return None so the caller falls back to
/// highlighted_line.
pub(crate) fn styled_row(row: &str, outcome: Option<ToolOutcome>) -> Option<Line<'static>> {
    let cyan = Style::new().fg(Color::Cyan);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::DarkGray);
    let green = Style::new().fg(Color::Rgb(78, 186, 101));
    let red = Style::new().fg(Color::Rgb(255, 107, 128));
    let glyph_style = match outcome {
        Some(ToolOutcome::Success) => green,
        Some(ToolOutcome::Error) => red,
        Some(ToolOutcome::Running) | None => cyan,
    };
    if let Some(rest) = row.strip_prefix("⏺ ") {
        // "⏺ Name(args...)" — bold the tool name, glyph by outcome, args plain.
        let (name, args) = match rest.find('(') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled("⏺", glyph_style),
            Span::raw(" "),
            Span::styled(name.to_string(), bold),
        ];
        if !args.is_empty() {
            spans.push(Span::raw(args.to_string()));
        }
        return Some(Line::from(spans));
    }
    if let Some(rest) = row.strip_prefix("  ⎿  ") {
        // Result row: the ⎿ gutter is dim. The body uses the default
        // foreground for success/running/none (stdout is plain text, not a
        // semantic signal — green-washing it drowns the color channel
        // reserved for errors), and red only for Error. An Edit diff
        // summary ("Added N lines, removed M lines") bolds the counts.
        let body_style = match outcome {
            Some(ToolOutcome::Error) => red,
            _ => Style::new().fg(Color::Reset),
        };
        let body_spans = if rest.starts_with("Added ") || rest.starts_with("Removed ") {
            bold_digit_spans(rest, body_style)
        } else {
            vec![Span::styled(rest.to_string(), body_style)]
        };
        let mut spans = vec![Span::styled("  ⎿  ", dim)];
        spans.extend(body_spans);
        return Some(Line::from(spans));
    }
    None
}

/// Split a summary string into spans where digit runs are BOLD (the count
/// numbers in "Added N lines, removed M lines") and the surrounding text
/// carries the base style. Bolds the counts in the Edit summary head;
/// other result summaries pass through as a single span.
fn bold_digit_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let bold = base.add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut acc = String::new();
    let mut in_digits = false;
    for (ch, is_digit) in text.chars().map(|c| (c, c.is_ascii_digit())) {
        if is_digit != in_digits {
            if !acc.is_empty() {
                let style = if in_digits { bold } else { base };
                spans.push(Span::styled(std::mem::take(&mut acc), style));
            }
            in_digits = is_digit;
        }
        acc.push(ch);
    }
    if !acc.is_empty() {
        let style = if in_digits { bold } else { base };
        spans.push(Span::styled(acc, style));
    }
    spans
}

/// Build the display rows for a tool result body (Ctrl+O collapse + gutter +
/// continuation indent). Pure (no App borrow) so the layout is unit-testable.
/// Returns one row per visible line as (tag, text, outcome, callid): the
/// summary row carries the ⎿ gutter + the result's outcome + the call_id (so
/// Ctrl+O can target it); continuation rows are 5-space indented with no
/// outcome; a long collapsed body ends in a ⎿ … Ctrl+O hint row. For an Edit
/// diff body the continuation rows carry a line-number + sigil gutter and a
/// dim "..." separates hunks; expanded bodies show every row; bodies at or
/// under COLLAPSE_SHOW never collapse.
pub(crate) fn result_body_rows(
    body: &str,
    call_id: &str,
    outcome: Option<ToolOutcome>,
    expanded: bool,
    is_diff: bool,
    width: u16,
) -> Vec<PlannedRow> {
    if is_diff && let Some(plan) = plan_diff_body(body, width) {
        return render_diff_plan(&plan, call_id, outcome, expanded);
    }
    let plain = TAG_PLAIN;
    // Trim trailing newlines before splitting so a body ending in a line
    // terminator (diffs, stdout) does not yield a spurious trailing empty
    // row that would both render a dim blank line and inflate the line count
    // (which would skew the collapse threshold and scroll totals).
    let trimmed = body.trim_end_matches('\n');
    let blines: Vec<&str> = trimmed.split('\n').collect();
    let avail = usize::from(width).saturating_sub(5);
    // Soft-wrap each logical line so a long stdout / read-content line shows
    // its full content across rows (an old renderer truncated
    // stdout to the pane edge — a limitation that lost the line tail; wrapping
    // preserves it). The COLLAPSED path pre-truncates the INPUT before wrap
    // (a maxChars = SHOW * width * 4 pre-cut) so a
    // pathological body (cat of a minified file, a 64MB binary dump) does not
    // make the count path wrap the whole thing; the remaining count is
    // estimated from the original length. The EXPANDED path wraps the full
    // body; the 4000-row viewable cap bounds the transcript as a whole.
    let total_chars = trimmed.chars().count();
    let (wrows, hidden) = if expanded {
        let mut w = Vec::new();
        for l in &blines {
            if avail == 0 {
                w.push((*l).to_string());
            } else {
                w.extend(crate::view::line_wrap::wrap_line(l, avail));
            }
        }
        (w, 0)
    } else {
        let max_chars = COLLAPSE_SHOW.saturating_mul(avail).saturating_mul(4);
        let (pre, was_truncated) = pretruncate_lines(&blines, max_chars);
        let mut w = Vec::new();
        for l in &pre {
            if avail == 0 {
                w.push(l.clone());
            } else {
                w.extend(crate::view::line_wrap::wrap_line(l, avail));
            }
        }
        let remaining_after_wrap = w.len().saturating_sub(COLLAPSE_SHOW);
        let estimated = if was_truncated {
            let est_total_rows = total_chars.max(1).div_ceil(avail.max(1));
            remaining_after_wrap.max(est_total_rows.saturating_sub(COLLAPSE_SHOW))
        } else {
            remaining_after_wrap
        };
        (w, estimated)
    };
    let shown = if expanded {
        wrows.len()
    } else {
        COLLAPSE_SHOW.min(wrows.len())
    };
    let mut out = Vec::with_capacity(shown + usize::from(hidden > 0));
    // Guard the empty-after-marker bug: a result whose body is empty or whose
    // first line is blank would render a bare "⎿ " with no text. Show a
    // "(no output)" placeholder instead so the marker always carries text.
    let first = if wrows.first().map(|s| s.trim().is_empty()).unwrap_or(true) {
        "(no output)".to_string()
    } else {
        wrows.first().cloned().unwrap_or_default()
    };
    out.push((
        plain,
        format!("  ⎿  {first}"),
        outcome,
        Some(call_id.to_string()),
        None,
    ));
    for l in wrows.iter().skip(1).take(shown.saturating_sub(1)) {
        // Continuation rows align to the result-text column (a 5-space indent
        // matching the 2-space ⎿ gutter + the ⎿ glyph + two spaces) with no
        // repeat ⎿ — continuations are indented rather than
        // re-glyphed every line, so the eye reads the body as one block.
        out.push((plain, format!("     {l}"), None, None, None));
    }
    if hidden > 0 {
        // In-app collapse form: the expand-hint
        // suffix is suppressed inside the virtual list (no terminal scrollback
        // to expand into) and leaves a clean "+N lines" tail aligned to the
        // body. Ctrl+O still expands (toggle_focused_result_expand),
        // the verbose inline hint text is dropped so a long transcript is not
        // littered with "Ctrl+O to expand" on every collapsed result.
        out.push((plain, format!("     … +{hidden} lines"), None, None, None));
    }
    out
}

/// Render a parsed diff plan to display rows: the summary row (⎿ gutter +
/// outcome + call_id) followed by the line-numbered diff rows with a dim
/// "..." gap between hunks, under the same COLLAPSE_SHOW + Ctrl+O expand
/// regime as a plain body. The gutter is right-aligned to the plan's max
/// line-number width; each diff row is <indent><num> <sigil> <text>.
fn render_diff_plan(
    plan: &[PlanRow],
    call_id: &str,
    outcome: Option<ToolOutcome>,
    _expanded: bool,
) -> Vec<PlannedRow> {
    // Diffs render in full (no line cap — a structured-diff list
    // has none, unlike stdout's truncated-content cap). Right-align every
    // line number to the widest one so the gutter column does not raggedly
    // shift as numbers grow. Min 1 so a single-digit diff still has a column.
    let max_w = plan
        .iter()
        .filter_map(|r| r.num)
        .map(|n| n.to_string().len())
        .max()
        .unwrap_or(1)
        .max(1);
    let mut out = Vec::with_capacity(plan.len() + 2);
    let mut framed = false;
    for row in plan.iter() {
        match row.kind {
            PlanKind::Summary => {
                let first = if row.text.trim().is_empty() {
                    "(no output)".to_string()
                } else {
                    row.text.clone()
                };
                out.push((
                    TAG_PLAIN,
                    format!("  ⎿  {first}"),
                    outcome,
                    Some(call_id.to_string()),
                    None,
                ));
                // Open the dashed frame below the summary line — the diff
                // block (hunks + gaps) sits inside a dashed frame
                // (dashed top + bottom, no left/right borders).
                out.push((TAG_DIFF_BORDER, String::new(), None, None, None));
                framed = true;
            }
            PlanKind::Gap => {
                // Dim "..." between hunks — the intersperse
                // separator for the omitted unchanged context.
                out.push((TAG_DIFF_HUNK, "...".to_string(), None, None, None));
            }
            PlanKind::Diff(kind) => {
                let sigil = match kind {
                    DiffKind::Add => '+',
                    DiffKind::Remove => '-',
                    DiffKind::Context => ' ',
                };
                let tag = match kind {
                    DiffKind::Add => TAG_DIFF_ADD,
                    DiffKind::Remove => TAG_DIFF_DEL,
                    DiffKind::Context => TAG_DIFF_CTX,
                };
                // num=None marks a soft-wrapped continuation row: a blank
                // gutter (spaces, number suppressed) so the wrap reads as one
                // block; the sigil is preserved across the continuation.
                // Layout: sigil + space + right-aligned number + space +
                // content — marker at col 0, no indent lead.
                let num_str = match row.num {
                    Some(n) => format!("{:>w$}", n, w = max_w),
                    None => " ".repeat(max_w),
                };
                out.push((
                    tag,
                    format!("{} {} {}", sigil, num_str, row.text),
                    None,
                    None,
                    row.word.clone(),
                ));
            }
        }
    }
    if framed {
        // Close the dashed frame below the last diff row.
        out.push((TAG_DIFF_BORDER, String::new(), None, None, None));
    }
    out
}

/// The display-row count a tool result body renders to. Delegates directly
/// to result_body_rows(...).len() so the count path and the render path share
/// ONE source of truth — the is_diff gate, the @@ header consumption, the
/// inter-hunk "..." gap insertion, and the COLLAPSE_SHOW + Ctrl+O math all
/// live in result_body_rows + plan_diff_body, never duplicated. A separate
/// A separately-maintained count copy was a drift class (an is_diff=false body
/// that happened to contain @@ made count consume the @@ as a diff while render
/// treated it as plain stdout → scroll/search-jump off by the gap count);
/// delegating closes it structurally, not by vigilance.
pub(crate) fn result_row_count(body: &str, expanded: bool, is_diff: bool, width: u16) -> usize {
    result_body_rows(body, "", None, expanded, is_diff, width).len()
}

/// Render a diff row: the marker + line-number gutter is colored by tag
/// (additions green, removals red, the inter-hunk gap + border dim). The
/// gutter is found via diff_gutter_skip (sigil at col 0). Additions and
/// removals also carry a tinted background across the WHOLE pane width
/// (a trailing run of bg-styled spaces pads the row so a changed line
/// reads as a solid green/red bar — the space-repeat fill).
/// When word-parts are present (an adjacent remove+add pair below the
/// change threshold), the content splits into word-spans so just the
/// changed words get a darker, more-saturated background. The width is
/// the pane inner width so the padding reaches the right edge.
pub(crate) fn diff_row(
    row: &str,
    tag: u8,
    width: u16,
    word: Option<&[WordDiffPart]>,
) -> Line<'static> {
    let dim = Style::new().fg(Color::DarkGray);
    // The dashed frame border (top + bottom of a structured-diff block):
    // a full-width dim dashed line, no gutter, no background.
    if tag == TAG_DIFF_BORDER {
        let line = "┄".repeat(usize::from(width).max(1));
        return Line::from(Span::styled(line, dim));
    }
    let green = Style::new()
        .fg(Color::Rgb(78, 186, 101))
        .bg(Color::Rgb(28, 38, 32));
    let red = Style::new()
        .fg(Color::Rgb(255, 107, 128))
        .bg(Color::Rgb(42, 28, 32));
    // Darker, more-saturated word backgrounds so a changed word stands out
    // against the dim line bar (darker per-word backgrounds for added/removed
    // words).
    let word_green = Style::new()
        .fg(Color::Rgb(180, 240, 200))
        .bg(Color::Rgb(46, 120, 70));
    let word_red = Style::new()
        .fg(Color::Rgb(255, 180, 190))
        .bg(Color::Rgb(120, 50, 56));
    // TAG_DIFF_HUNK marks the inter-hunk "..." gap (the @@ header is consumed
    // as metadata by plan_diff_body, never emitted as a row), so it renders
    // dim rather than the old cyan @@ rendering.
    let style = match tag {
        TAG_DIFF_ADD => green,
        TAG_DIFF_DEL => red,
        TAG_DIFF_HUNK => dim,
        _ => dim,
    };
    let content_start = crate::selection::diff_gutter_skip(row).min(row.len());
    let mut spans: Vec<Span<'static>> = Vec::new();
    // The marker + line-number gutter (before the content) inherits the row
    // background so the tint starts at col 0; its foreground is the row's
    // sigil color (green/red/dim). Marker at col 0, no indent lead.
    if content_start > 0 {
        spans.push(Span::styled(row[..content_start].to_string(), style));
    }
    if let Some(parts) = word
        && matches!(tag, TAG_DIFF_ADD | TAG_DIFF_DEL)
    {
        spans.extend(word_diff_spans(parts, tag, style, word_green, word_red));
    } else {
        spans.push(Span::styled(row[content_start..].to_string(), style));
    }

    // Pad to the pane width with bg-styled spaces so the bar fills the row
    // (only add/remove carry a background; context + the gap stay bare).
    if let Some(bg) = style.bg {
        let content_w = UnicodeWidthStr::width(row) as u16;
        if let Some(pad) = (width as usize).checked_sub(content_w as usize)
            && pad > 0
        {
            spans.push(Span::styled(" ".repeat(pad), Style::new().bg(bg)));
        }
    }
    Line::from(spans)
}

/// The per-word spans for a word-diff content row. On a remove row show the
/// equal and removed parts; on an add row show the equal and added parts
/// (the shouldShow filter — remove shows equal+removed, add shows equal+added).
/// Changed words get the darker word
/// background, equal runs the line-bar background. The style arg is the
/// row's line-bar style (green for add, red for remove); word_green and
/// word_red are the darker per-word styles applied to changed words.
fn word_diff_spans(
    parts: &[WordDiffPart],
    tag: u8,
    style: Style,
    word_green: Style,
    word_red: Style,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for part in parts {
        let show = match tag {
            TAG_DIFF_DEL => !part.added,
            TAG_DIFF_ADD => !part.removed,
            _ => false,
        };
        if !show || part.value.is_empty() {
            continue;
        }
        let changed = (tag == TAG_DIFF_ADD && part.added) || (tag == TAG_DIFF_DEL && part.removed);
        let ws = if changed {
            match tag {
                TAG_DIFF_ADD => word_green,
                TAG_DIFF_DEL => word_red,
                _ => style,
            }
        } else {
            style
        };
        out.push(Span::styled(part.value.clone(), ws));
    }
    out
}

#[cfg(test)]
mod tests;
