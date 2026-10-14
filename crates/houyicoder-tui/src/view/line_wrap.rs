//! Display-width-aware soft-wrap for a single logical line, shared by the
//! transcript's three long-line content kinds (diff bodies, agent markdown,
//! bash stdout) so they wrap consistently and the count path can match the
//! render path. A presentation-layer concern, backed by unicode-width (display
//! columns, not byte/char count) + unicode-segmentation graphemes (so a
//! multi-byte token breaks on a grapheme boundary, never mid-codepoint).
//!
//! Greedy word-boundary wrap: accumulate tokens (words + the whitespace runs
//! between them, preserved) until the next token would exceed the available
//! width, then flush. A single token wider than the available width is
//! hard-broken on grapheme boundaries (no overflow). avail == 0 means "do not
//! wrap" (the pane width is unknown / too narrow) — returns the line whole so
//! the caller falls back to the terminal's truncation, never panics.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::borrow::Cow;
use std::collections::HashSet;
use std::mem::take;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Truncate a string so its display width is at most max_width columns,
/// appending an ellipsis when truncation occurs. Width-aware: a CJK
/// ideograph (width 2) is counted correctly, and the cut point never
/// lands inside a multi-byte character. max_width 0 returns empty.
///
/// This is the single shared truncation helper for the view layer. All
/// one-line previews (trajectory titles, palette help, queue items, resume
/// picker rows) route through here so no code path can panic on a
/// multi-byte boundary.
pub(crate) fn truncate_width(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if s.is_empty() {
        return String::new();
    }
    let total = UnicodeWidthStr::width(s);
    if total <= max_width {
        return s.to_string();
    }
    // max_width 1: only the ellipsis fits.
    if max_width == 1 {
        return "\u{2026}".to_string();
    }
    let target = max_width - 1; // reserve 1 column for the ellipsis
    let mut acc = 0usize;
    let mut out = String::new();
    for grapheme in s.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme);
        if acc + width > target {
            break;
        }
        acc += width;
        out.push_str(grapheme);
    }
    out.push('\u{2026}');
    out
}

/// Split a single logical line into display rows each no wider than the avail
/// display columns. Returns at least one row (the input unchanged when it
/// fits, when avail is 0, or when the input is empty). Trailing whitespace is
/// trimmed per wrapped row so a line does not end in a space that would push
/// the next word to its own row.
pub fn wrap_line(text: &str, avail: usize) -> Vec<String> {
    if avail == 0 || text.is_empty() {
        return vec![text.to_string()];
    }
    if UnicodeWidthStr::width(text) <= avail {
        return vec![text.to_string()];
    }
    let mut end = 0usize;
    let cells: Vec<WrapCell> = text
        .graphemes(true)
        .map(|grapheme| {
            end += grapheme.len();
            WrapCell {
                text: grapheme.to_string(),
                style: Style::default(),
                end,
            }
        })
        .collect();
    let rows = wrap_cells(text, cells, avail);
    if rows.is_empty() {
        vec![String::new()]
    } else {
        rows.into_iter()
            .map(|row| row.into_iter().map(|cell| cell.text).collect())
            .collect()
    }
}

/// Width-wrap a plain-text block the way a single Ink Text node wraps a user
/// prompt: prefix_first (the angle-bracket lead) is prepended to the first
/// row only; wrapped continuation rows hang at the prefix width so they align
/// with the text after the lead instead of jumping to column 0. Splitting on
/// newlines first preserves explicit line breaks, then each logical line wraps
/// to the pane width minus the prefix.
///
/// The cap argument enables a head+tail display cap (a
/// piped-in large prompt is capped at 10 000 chars — head 2 500 + tail 2 500 plus an
/// ellipsis +N lines marker — so a huge paste does not make the renderer
/// iterate the full text each frame; the model still receives the full text,
/// only the display is capped). Returns at least one row.
pub fn wrap_plain_block(
    text: &str,
    prefix_first: &str,
    width: u16,
    cap: Option<usize>,
) -> Vec<String> {
    const HEAD: usize = 2_500;
    const TAIL: usize = 2_500;
    let display: Cow<str> = match cap {
        Some(max) if text.chars().count() > max => {
            let chars: Vec<char> = text.chars().collect();
            let head: String = chars[..HEAD].iter().collect();
            let tail_start = chars.len() - TAIL;
            let tail: String = chars[tail_start..].iter().collect();
            let hidden = chars[HEAD..tail_start]
                .iter()
                .filter(|c| **c == '\n')
                .count();
            format!("{head}\n… +{hidden} lines …\n{tail}").into()
        }
        _ => text.into(),
    };
    let prefix_w = UnicodeWidthStr::width(prefix_first);
    let indent = " ".repeat(prefix_w);
    let avail = (width as usize).saturating_sub(prefix_w).max(1);
    let mut out: Vec<String> = Vec::new();
    let mut first_row = true;
    for logical in display.split('\n') {
        for wrapped in wrap_line(logical, avail) {
            if first_row {
                out.push(format!("{prefix_first}{wrapped}"));
                first_row = false;
            } else {
                out.push(format!("{indent}{wrapped}"));
            }
        }
    }
    if out.is_empty() {
        out.push(prefix_first.to_string());
    }
    out
}

/// Width-wrap a plain-text block with a per-row prefix (the thought-expand
/// rows: every logical line indented by prefix, wrapped to the pane width
/// minus the prefix width so it cannot overflow the pane). No char cap —
/// reasoning is bounded by the model. Returns at least one row.
pub fn wrap_indented_block(text: &str, prefix: &str, width: u16) -> Vec<String> {
    let avail = (width as usize).saturating_sub(UnicodeWidthStr::width(prefix));
    let mut out: Vec<String> = Vec::new();
    for logical in text.split('\n') {
        for wrapped in wrap_line(logical, avail) {
            out.push(format!("{prefix}{wrapped}"));
        }
    }
    if out.is_empty() {
        out.push(prefix.to_string());
    }
    out
}

/// Soft-wrap a STYLED line (a ratatui Line of Spans) to the available width,
/// preserving each span's style across the wrapped rows. Used by the agent
/// markdown renderer (which emits Line<Vec<Span>>), so long agent lines wrap
/// the same way plain diff/stdout lines do — one wrap helper, three content
/// kinds. The wrap is word-boundary greedy (a space token at the wrap point
/// is dropped); a single non-space token wider than the column is
/// hard-broken on grapheme boundaries. avail == 0 returns the line whole.
pub fn wrap_styled_line(line: Line<'static>, avail: usize) -> Vec<Line<'static>> {
    if avail == 0 {
        return vec![line];
    }
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    if UnicodeWidthStr::width(text.as_str()) <= avail {
        return vec![line];
    }
    let mut end = 0usize;
    let mut cells = Vec::new();
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            end += grapheme.len();
            cells.push(WrapCell {
                text: grapheme.to_string(),
                style: span.style,
                end,
            });
        }
    }
    let rows = wrap_cells(&text, cells, avail);
    if rows.is_empty() {
        vec![Line::default()]
    } else {
        rows.into_iter().map(rebuild_spans).collect()
    }
}

#[derive(Clone)]
struct WrapCell {
    text: String,
    style: Style,
    end: usize,
}

fn wrap_cells(text: &str, cells: Vec<WrapCell>, avail: usize) -> Vec<Vec<WrapCell>> {
    let breaks: HashSet<usize> = unicode_linebreak::linebreaks(text)
        .map(|(offset, _)| offset)
        .collect();
    let mut rows = Vec::new();
    let mut start = 0usize;
    let mut first = true;
    while start < cells.len() {
        if !first {
            while start < cells.len() && cells[start].text == " " {
                start += 1;
            }
        }
        if start == cells.len() {
            break;
        }
        let mut width = 0usize;
        let mut cursor = start;
        let mut last_break = None;
        while cursor < cells.len() {
            let cell = &cells[cursor];
            let cell_width = UnicodeWidthStr::width(cell.text.as_str());
            let space = cell.text == " ";
            if width + cell_width > avail && width > 0 && !space {
                break;
            }
            width += cell_width;
            cursor += 1;
            if breaks.contains(&cell.end) {
                last_break = Some(cursor);
            }
        }
        let end = if cursor == cells.len() {
            cursor
        } else {
            last_break
                .filter(|end| *end > start)
                .unwrap_or(cursor.max(start + 1))
        };
        let row = trim_cells(cells[start..end].to_vec());
        if !row.is_empty() {
            rows.push(row);
        }
        start = end;
        first = false;
    }
    rows
}

fn trim_cells(mut row: Vec<WrapCell>) -> Vec<WrapCell> {
    while row.last().is_some_and(|cell| cell.text == " ") {
        row.pop();
    }
    row
}

fn rebuild_spans(row: Vec<WrapCell>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut acc = String::new();
    let mut acc_style: Option<Style> = None;
    for cell in row {
        if acc_style != Some(cell.style) {
            if !acc.is_empty() {
                spans.push(Span::styled(take(&mut acc), acc_style.unwrap_or_default()));
            }
            acc_style = Some(cell.style);
        }
        acc.push_str(&cell.text);
    }
    if !acc.is_empty() {
        spans.push(Span::styled(acc, acc_style.unwrap_or_default()));
    }
    Line::from(spans)
}

/// The largest scroll offset a wrapped body needs to bring its last row
/// into a viewport of height rows: the wrapped row count minus the viewport,
/// or zero when the body fits. Shared by the detail views so the offset a
/// key press clamps against matches the rows the renderer produced.
pub(crate) fn max_scroll_offset(body: &[Line<'static>], width: u16, height: u16) -> u16 {
    let rendered_rows: usize = body
        .iter()
        .cloned()
        .map(|line| wrap_styled_line(line, width as usize).len())
        .sum();
    u16::try_from(rendered_rows.saturating_sub(height as usize)).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests;
