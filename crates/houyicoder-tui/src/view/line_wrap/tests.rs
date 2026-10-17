use super::*;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

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
        UnicodeWidthStr::width(out.as_str()) <= 5,
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
    assert!(UnicodeWidthStr::width(out.as_str()) <= 10);
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
fn test_truncate_keeps_grapheme() {
    let out = truncate_width("\u{1f469}\u{200d}\u{1f4bb}xy", 3);
    assert_eq!(out, "\u{1f469}\u{200d}\u{1f4bb}\u{2026}");
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
            UnicodeWidthStr::width(out.as_str()) <= max,
            "max={max} width={}: [{out}]",
            UnicodeWidthStr::width(out.as_str())
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

/// Leading indentation wider than the available width is discarded
/// without emitting an empty row.
#[test]
fn test_leading_spaces_wider_dropped() {
    let rows = wrap_line("        self", 5);
    assert_eq!(rows, vec!["self".to_string()]);
    assert!(
        !rows[0].is_empty(),
        "leading spaces must not produce an empty row: {rows:?}"
    );
}

/// Leading spaces that fit within avail are preserved on a content row
/// that WRAPS — the indentation survives the flush because the row
/// carries real content. This exercises the wrap loop (the input is
/// wider than avail), not the early-return path.
#[test]
fn test_indent_survives_wrap() {
    // 2-space indent + content that forces a wrap at avail 20. The first
    // row must keep the 2-space indent (Python/YAML semantics depend on
    // it); the regression dropped all leading spaces unconditionally.
    let rows = wrap_line("  self.foo() and more text here", 20);
    assert_eq!(
        rows[0], "  self.foo() and",
        "indent must survive the wrap: {rows:?}"
    );
    assert!(rows.len() > 1);
    // No phantom empty rows anywhere.
    assert!(
        !rows.iter().any(|r| r.trim().is_empty()),
        "no phantom empty rows: {rows:?}"
    );
}

/// A deeply-indented source line (16 spaces + short content) at a
/// narrow pane drops the excess indentation (it would be a phantom
/// blank row) and keeps the content.
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
fn test_cjk_uses_width() {
    let rows = wrap_line(
        "\u{8bf4}\u{660e} alpha \u{4e2d}\u{6587}\u{5c3e}\u{53e5}\u{5e94}\u{8be5}\u{586b}\u{6ee1}\u{ff09}\u{3002}",
        24,
    );
    assert!(
        UnicodeWidthStr::width(rows[0].as_str()) > UnicodeWidthStr::width("\u{8bf4}\u{660e} alpha"),
        "the CJK tail should use the first row's remaining width: {rows:?}"
    );
    assert!(
        rows.iter()
            .skip(1)
            .all(|row| { !row.starts_with(['\u{ff09}', '\u{ff0c}', '\u{3002}']) }),
        "closing punctuation must not lead a continuation row: {rows:?}"
    );
}

#[test]
fn test_style_keeps_wraps() {
    let text = "\u{8bf4}\u{660e} alpha \u{4e2d}\u{6587}\u{5c3e}\u{53e5}\u{5e94}\u{8be5}\u{586b}\u{6ee1}\u{ff09}\u{3002}";
    let plain_rows = wrap_line(text, 24);
    let styled = Line::from(vec![
        Span::raw("\u{8bf4}\u{660e} alpha "),
        Span::styled(
            "\u{4e2d}\u{6587}\u{5c3e}\u{53e5}",
            Style::new().fg(Color::Cyan),
        ),
        Span::raw("\u{5e94}\u{8be5}\u{586b}\u{6ee1}\u{ff09}\u{3002}"),
    ]);
    let styled_rows: Vec<String> = wrap_styled_line(styled, 24)
        .into_iter()
        .map(|row| row.spans.iter().map(|span| span.content.as_ref()).collect())
        .collect();
    assert_eq!(styled_rows, plain_rows);
}

#[test]
fn test_styled_wraps_preserves_style() {
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
/// on the first row, continuation rows hung at the prefix width, and a
/// long line wraps to the pane width minus the prefix.
#[test]
fn test_plain_block_wraps_narrow() {
    let rows = wrap_plain_block("alpha bravo charlie delta", "> ", 12, Some(10_000));
    // "> alpha bravo" is 13 wide > 12 → wraps after "alpha".
    assert_eq!(rows[0], "> alpha");
    // Continuation rows hang at the prefix width (2 spaces for "> ").
    assert!(rows.len() > 1);
    assert!(rows[1].starts_with("  "), "hang-indent: {}", rows[1]);
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

/// A list that fits its body renders from the first row, so short lists are
/// unaffected by the cursor window.
#[test]
fn test_cursor_window_fits() {
    assert_eq!(cursor_window_offset(2, 5, 5), 0, "the list fits");
    assert_eq!(
        cursor_window_offset(4, 5, 6),
        0,
        "the list has room to spare"
    );
}

/// A cursor below the window scrolls the body by exactly as much as the
/// selection passed its last row, so the selected row lands on the bottom edge.
#[test]
fn test_cursor_window_follows() {
    assert_eq!(
        cursor_window_offset(4, 20, 5),
        0,
        "the first window holds it"
    );
    assert_eq!(cursor_window_offset(5, 20, 5), 1, "one row past the window");
    assert_eq!(
        cursor_window_offset(9, 20, 5),
        5,
        "the row below the window"
    );
}

/// A selection at the end of the list shows the tail: the window never runs
/// past the last row into empty space.
#[test]
fn test_cursor_window_tail() {
    assert_eq!(cursor_window_offset(19, 20, 5), 15, "the tail is the floor");
    assert_eq!(cursor_window_offset(30, 20, 5), 15, "a cursor past the end");
}

/// A body with no height to draw into cannot be scrolled, so the offset stays
/// at the top rather than wrapping around a zero-height subtraction.
#[test]
fn test_cursor_window_no_height() {
    assert_eq!(cursor_window_offset(9, 20, 0), 0);
}
