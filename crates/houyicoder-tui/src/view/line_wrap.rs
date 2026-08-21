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
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate a string so its display width is at most max_width columns,
/// appending an ellipsis when truncation occurs. Width-aware: a CJK
/// ideograph (width 2) is counted correctly, and the cut point never
/// lands inside a multi-byte character. max_width 0 returns empty.
///
/// This is the single shared truncation helper for the view layer. All
/// one-line previews (trajectory titles, palette help, queue items, resume
/// picker rows) route through here so no code path can panic on a
/// multi-byte boundary.
pub fn truncate_width(s: &str, max_width: usize) -> String {
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
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc + cw > target {
            break;
        }
        acc += cw;
        out.push(ch);
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
    // Tokens: alternating runs of non-space and ASCII-space, so the whitespace
    // between words is preserved as its own token (a wrap point lands between
    // a word and the following space, never inside the space run).
    let mut tokens: Vec<&str> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let is_space = bytes[i] == b' ';
        let start = i;
        while i < bytes.len() && (bytes[i] == b' ') == is_space {
            i += 1;
        }
        tokens.push(&text[start..i]);
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for tok in tokens {
        let tok_w = UnicodeWidthStr::width(tok);
        // A single non-space token wider than avail: hard-break it on grapheme
        // boundaries so it cannot overflow the column.
        if tok_w > avail && !tok.chars().all(|c| c == ' ') {
            if !cur.is_empty() {
                lines.push(trim_trailing(&cur));
                cur.clear();
                cur_w = 0;
            }
            let mut chunk = String::new();
            let mut chunk_w = 0usize;
            for g in tok.graphemes(true) {
                let gw = UnicodeWidthStr::width(g);
                if chunk_w + gw > avail && !chunk.is_empty() {
                    lines.push(chunk.clone());
                    chunk.clear();
                    chunk_w = 0;
                }
                chunk.push_str(g);
                chunk_w += gw;
            }
            if !chunk.is_empty() {
                cur = chunk;
                cur_w = chunk_w;
            }
            continue;
        }
        // Drop a leading space token at the start of a row (cur is empty):
        // indentation wider than the column is trimmed, not kept as an empty
        // row. Without this a line like "        self" (8-space indent) at
        // avail 5 would produce an empty first row (the spaces trimmed by
        // trim_trailing) then "self" on the second row — a phantom blank row
        // + a premature break.
        if cur.is_empty() && tok.chars().all(|c| c == ' ') {
            continue;
        }
        // Greedy fit: flush the current row if this token would overflow.
        if cur_w + tok_w > avail && !cur.is_empty() {
            lines.push(trim_trailing(&cur));
            cur.clear();
            cur_w = 0;
            // Drop a leading space token at the start of a fresh row.
            if tok.chars().all(|c| c == ' ') {
                continue;
            }
        }
        cur.push_str(tok);
        cur_w += tok_w;
    }
    if !cur.is_empty() {
        lines.push(trim_trailing(&cur));
    }
    if lines.is_empty() {
        vec![text.to_string()]
    } else {
        lines
    }
}

/// Trim only trailing ASCII spaces (not internal, not leading) so a wrapped
/// row does not end in a space that would waste the wrap point.
fn trim_trailing(s: &str) -> String {
    s.trim_end_matches(' ').to_string()
}

/// Width-wrap a plain-text block the way a single Ink Text node wraps a user
/// prompt: prefix_first (the angle-bracket lead) is prepended once to the
/// whole text, then the combined string is wrapped as one logical block —
/// the first row carries the prefix, wrapped continuation rows line up at
/// column 0 (no per-row prefix). Splitting on newlines first preserves
/// explicit line breaks, then each logical line wraps to the pane width.
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
    let display: std::borrow::Cow<str> = match cap {
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
    let full = format!("{prefix_first}{display}");
    let avail = width as usize;
    let mut out: Vec<String> = Vec::new();
    for logical in full.split('\n') {
        for wrapped in wrap_line(logical, avail) {
            out.push(wrapped);
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
    // Flatten to (grapheme, style) cells, then group into tokens: maximal runs
    // of same-(style, space-class) graphemes. A space token is its own token
    // so a wrap lands between a word and the following space.
    let cells: Vec<(String, Style)> = line
        .spans
        .iter()
        .flat_map(|s| {
            s.content
                .graphemes(true)
                .map(move |g| (g.to_string(), s.style))
        })
        .collect();
    let total_w: usize = cells
        .iter()
        .map(|(g, _)| UnicodeWidthStr::width(g.as_str()))
        .sum();
    if total_w <= avail {
        return vec![line.clone()];
    }
    let mut tokens: Vec<(String, Style, bool)> = Vec::new(); // (text, style, is_space)
    let mut acc = String::new();
    let mut acc_style: Option<Style> = None;
    let mut acc_space = false;
    for (g, style) in cells {
        let is_space = g == " ";
        if acc_style != Some(style) || acc_space != is_space || acc.is_empty() {
            if !acc.is_empty() {
                tokens.push((
                    std::mem::take(&mut acc),
                    acc_style.unwrap_or_default(),
                    acc_space,
                ));
            }
            acc_style = Some(style);
            acc_space = is_space;
        }
        acc.push_str(&g);
    }
    if !acc.is_empty() {
        tokens.push((acc, acc_style.unwrap_or_default(), acc_space));
    }
    // Greedy fit.
    let mut rows: Vec<Vec<(String, Style)>> = Vec::new();
    let mut cur: Vec<(String, Style)> = Vec::new();
    let mut cur_w = 0usize;
    for (tok, style, is_space) in tokens {
        let tok_w = UnicodeWidthStr::width(tok.as_str());
        // Hard-break a single non-space token wider than avail on grapheme
        // boundaries (cannot overflow the column).
        if tok_w > avail && !is_space {
            if !cur.is_empty() {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            let mut chunk = String::new();
            let mut chunk_w = 0usize;
            for g in tok.graphemes(true) {
                let gw = UnicodeWidthStr::width(g);
                if chunk_w + gw > avail && !chunk.is_empty() {
                    rows.push(vec![(chunk.clone(), style)]);
                    chunk.clear();
                    chunk_w = 0;
                }
                chunk.push_str(g);
                chunk_w += gw;
            }
            if !chunk.is_empty() {
                cur.push((chunk, style));
                cur_w = chunk_w;
            }
            continue;
        }
        // Drop a leading space token at the start of a row (cur is empty):
        // indentation wider than the column is trimmed, not kept as a phantom
        // empty row. Matches the plain wrap_line path.
        if cur.is_empty() && is_space {
            continue;
        }
        // Greedy: flush if this token would overflow, dropping a leading
        // space token at the fresh row's start.
        if cur_w + tok_w > avail && !cur.is_empty() {
            rows.push(trim_trailing_styled(std::mem::take(&mut cur)));
            cur_w = 0;
            if is_space {
                continue;
            }
        }
        cur.push((tok, style));
        cur_w += tok_w;
    }
    if !cur.is_empty() {
        rows.push(trim_trailing_styled(cur));
    }
    if rows.is_empty() {
        vec![line.clone()]
    } else {
        rows.into_iter().map(rebuild_spans).collect()
    }
}

/// Drop a trailing space token from a styled-cell row (matches trim_trailing
/// for the plain path) so a wrapped row does not end in a space.
fn trim_trailing_styled(mut row: Vec<(String, Style)>) -> Vec<(String, Style)> {
    while row
        .last()
        .map(|(t, _)| t.chars().all(|c| c == ' '))
        .unwrap_or(false)
    {
        row.pop();
    }
    row
}

/// Rebuild a ratatui Line from styled cells by merging adjacent same-style
/// graphemes into Spans (fewer spans = lighter render + cleaner selection).
fn rebuild_spans(row: Vec<(String, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut acc = String::new();
    let mut acc_style: Option<Style> = None;
    for (text, style) in row {
        if acc_style != Some(style) {
            if !acc.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut acc),
                    acc_style.unwrap_or_default(),
                ));
            }
            acc_style = Some(style);
        }
        acc.push_str(&text);
    }
    if !acc.is_empty() {
        spans.push(Span::styled(acc, acc_style.unwrap_or_default()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_width_keeps_short() {
        assert_eq!(truncate_width("short", 10), "short");
    }

    #[test]
    fn test_truncate_width_exact_fit() {
        assert_eq!(truncate_width("abcde", 5), "abcde");
    }

    #[test]
    fn test_truncate_width_adds_ellipsis() {
        let out = truncate_width("abcdefghij", 5);
        assert!(out.ends_with('\u{2026}'));
        assert!(
            unicode_width::UnicodeWidthStr::width(out.as_str()) <= 5,
            "width overflow: {out}"
        );
    }

    #[test]
    fn test_truncate_width_zero_empty() {
        assert_eq!(truncate_width("anything", 0), "");
    }

    #[test]
    fn test_truncate_one_col_ellipsis() {
        assert_eq!(truncate_width("abc", 1), "\u{2026}");
    }

    #[test]
    fn test_truncate_cjk_no_panic() {
        // The crash that motivated this helper: a byte-slice truncation
        // landed inside a 3-byte CJK codepoint and panicked. A width-aware
        // helper never cuts mid-codepoint.
        let title = "你在 dev 上改动吧";
        let out = truncate_width(title, 10);
        assert!(unicode_width::UnicodeWidthStr::width(out.as_str()) <= 10);
        // Must not panic — that IS the test.
    }

    #[test]
    fn test_truncate_cjk_counted() {
        // Each CJK ideograph is display width 2; at max 5 we keep 2
        // ideographs (width 4) + ellipsis (width 1) = 5.
        let out = truncate_width("\u{4e2d}\u{6587}\u{6d4b}\u{8bd5}", 5);
        assert_eq!(out, "\u{4e2d}\u{6587}\u{2026}");
    }

    #[test]
    fn test_truncate_width_never_exceeds() {
        for (s, max) in [
            ("short", 10),
            ("a very long help string", 10),
            ("a very long help string", 38),
            ("a very long help string", 3),
            ("a very long help string", 2),
            ("a very long help string", 0),
            ("\u{4e2d}\u{6587}\u{6d4b}\u{8bd5}\u{4e2d}\u{6587}", 7),
        ] {
            let out = truncate_width(s, max);
            assert!(
                unicode_width::UnicodeWidthStr::width(out.as_str()) <= max,
                "max={max} width={}: [{out}]",
                unicode_width::UnicodeWidthStr::width(out.as_str())
            );
        }
    }

    #[test]
    fn test_fits_returns_one_row() {
        assert_eq!(wrap_line("short", 80), vec!["short".to_string()]);
        assert_eq!(wrap_line("short", 5), vec!["short".to_string()]);
    }

    #[test]
    fn test_avail_zero_no_wrap() {
        // avail 0 = unknown width → do not wrap (caller truncates).
        assert_eq!(wrap_line("anything", 0), vec!["anything".to_string()]);
    }

    #[test]
    fn test_empty_returns_one_row() {
        assert_eq!(wrap_line("", 80), vec!["".to_string()]);
    }

    #[test]
    fn test_wraps_at_word_boundary() {
        // "aa bb cc" at avail 5 → "aa bb" (5) then "cc". The space after bb
        // is trimmed so the row is exactly "aa bb".
        let rows = wrap_line("aa bb cc", 5);
        assert_eq!(rows, vec!["aa bb".to_string(), "cc".to_string()]);
    }

    #[test]
    fn test_preserves_internal_spaces() {
        // Two spaces between words are preserved inside a row.
        let rows = wrap_line("a  b cdefghij", 6);
        assert_eq!(rows[0], "a  b");
        // "cdefghij" (8) > 6 → hard-broken: "cdefgh" + "ij".
        assert!(rows.len() >= 3);
        assert_eq!(rows.last().unwrap(), "ij");
    }

    #[test]
    fn test_hard_breaks_wide_token() {
        // A single token wider than avail breaks on grapheme boundaries, never
        // overflows.
        let rows = wrap_line("xxxxxxxxxx", 4);
        assert_eq!(
            rows,
            vec!["xxxx", "xxxx", "xx"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    /// A line with leading indentation wider than avail drops the excess
    /// spaces instead of producing a phantom empty row. Without the fix
    /// "        self" (8-space indent) at avail 5 produced ["", "self"] —
    /// an empty first row (spaces trimmed by trim_trailing) then "self".
    /// The fix drops leading spaces at the start of a row, so the result
    /// is ["self"] — no phantom blank, no premature break.
    #[test]
    fn test_leading_spaces_wider_dropped() {
        let rows = wrap_line("        self", 5);
        assert_eq!(rows, vec!["self".to_string()]);
        // No empty first row.
        assert!(
            !rows[0].is_empty(),
            "leading spaces must not produce an empty row: {rows:?}"
        );
    }

    /// Leading spaces that fit within avail are preserved (indentation is
    /// only trimmed when it would overflow the column).
    #[test]
    fn test_leading_spaces_within_avail() {
        let rows = wrap_line("  self", 10);
        assert_eq!(rows, vec!["  self".to_string()]);
    }

    /// A deeply-indented source line (16 spaces + short content) at a
    /// narrow pane wraps the content, not the indentation. The excess
    /// indentation is dropped so the user sees the content, not a wall of
    /// spaces.
    #[test]
    fn test_deep_indent_drops_excess() {
        let rows = wrap_line("                self.value", 10);
        // The 16 spaces are dropped (wider than avail); "self.value"
        // (10) fits exactly.
        assert_eq!(rows, vec!["self.value".to_string()]);
        assert!(
            !rows.iter().any(|r| r.trim().is_empty()),
            "no phantom empty rows: {rows:?}"
        );
    }

    #[test]
    fn test_multibyte_breaks_on_grapheme() {
        // A wide CJK ideograph (display width 2); at avail 3, three of them
        // (total width 6) hard-break to one-per-row (width 2), never
        // mid-codepoint.
        let wide = "\u{4e2d}";
        let rows = wrap_line(&format!("{0}{0}{0}", wide), 3);
        assert_eq!(
            rows,
            vec![wide.to_string(), wide.to_string(), wide.to_string()]
        );
    }

    #[test]
    fn test_styled_wraps_preserves_style() {
        use ratatui::style::{Color, Modifier, Style};
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let plain = Style::new().fg(Color::Reset);
        // "boldword rest" where "boldword" is bold, " rest" plain. At avail 9:
        // "boldword" (8) fits, " rest" (5) → 8+5=13 > 9 → wrap → "boldword" +
        // "rest" (the leading space dropped). The bold span survives on the
        // first row, plain on the second.
        let line = Line::from(vec![
            Span::styled("boldword".to_string(), bold),
            Span::styled(" rest".to_string(), plain),
        ]);
        let rows = wrap_styled_line(line, 9);
        assert_eq!(rows.len(), 2);
        let r0: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let r1: String = rows[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(r0, "boldword");
        assert_eq!(r1, "rest");
        // Style preserved: row 0's first span is bold, row 1's is plain.
        assert_eq!(rows[0].spans[0].style.add_modifier, bold.add_modifier);
        assert_eq!(rows[1].spans[0].style.fg, plain.fg);
    }

    /// A plain block under the char cap renders whole: the angle-bracket lead
    /// on the first row, continuation rows at column 0, and a long line wraps
    /// to the pane width.
    #[test]
    fn test_plain_block_wraps_narrow() {
        let rows = wrap_plain_block("alpha bravo charlie delta", "> ", 12, Some(10_000));
        // "> alpha bravo" is 13 wide > 12 → wraps after "alpha".
        assert_eq!(rows[0], "> alpha");
        // Continuation rows line up at column 0 (no per-row prefix).
        assert!(rows.len() > 1);
        assert!(!rows[1].starts_with('>'));
    }

    /// A plain block over the char cap keeps a head + tail with an ellipsis
    /// +N lines marker in the middle — only the DISPLAY is capped; the marker
    /// row is itself a logical line so it counts as a row.
    #[test]
    fn test_plain_block_caps_long() {
        let big = "line of text\n".repeat(2500); // 12500 chars, 2500 newlines
        let rows = wrap_plain_block(&big, "> ", 80, Some(10_000));
        let combined = rows.join("\n");
        assert!(
            combined.contains("… +") && combined.contains("lines …"),
            "capped prompt must show the hidden-lines marker: {combined}"
        );
        // Only the display is capped: combined is head + tail + marker, far
        // shorter than the full text.
        assert!(
            combined.len() < big.len(),
            "display must be shorter than the full text: {} vs {}",
            combined.len(),
            big.len()
        );
    }

    /// An indented block (thought-expand rows) wraps each logical line to the
    /// pane width minus the two-space indent, prefix on every row.
    #[test]
    fn test_indented_block_wraps() {
        let rows = wrap_indented_block("alpha bravo charlie delta", "  ", 12);
        assert_eq!(rows[0], "  alpha");
        assert!(rows.len() > 1);
        assert!(rows.iter().all(|r| r.starts_with("  ")));
    }
}
