use super::*;

fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
    Rect::new(x, y, w, h)
}

fn row(s: &str) -> (u8, String) {
    (TAG_PLAIN, s.to_string())
}

fn srow(s: &str) -> (u8, String) {
    (TAG_SPINNER, s.to_string())
}

#[test]
fn test_selection_bounds_normalize() {
    let mut s = Selection::default();
    s.start(5, 2);
    s.update(1, 4);
    // Normalized by content row: anchor (5,2) comes first, focus (1,4) last.
    assert_eq!(s.bounds(), Some(((5, 2), (1, 4))));
}

#[test]
fn test_extract_single_row() {
    let rows = vec![row("hello world")];
    let mut s = Selection::default();
    s.start(0, 0); // anchor at col 0
    s.update(4, 0); // focus at col 4
    s.finish();
    let t = extract_text(&rows, rect(0, 0, 80, 1), &s);
    assert_eq!(t, "hello");
}

#[test]
fn test_extract_spans_rows() {
    let rows = vec![row("ab"), row("cd")];
    let mut s = Selection::default();
    s.start(1, 0); // anchor col 1 row 0
    s.update(1, 1); // focus col 1 row 1
    s.finish();
    let t = extract_text(&rows, rect(0, 0, 80, 2), &s);
    // row 0: cols 1..end => "b"; row 1: cols 0..2 => "cd"
    assert_eq!(t, "b\ncd");
}

#[test]
fn test_extract_down_left_drag() {
    // anchor top-right (col 5 row 0), focus bottom-left (col 1 row 2):
    // row 0 = cols 5..end, row 1 = full, row 2 = cols 0..2.
    let rows = vec![row("abcdef"), row("xyz"), row("pq")];
    let mut s = Selection::default();
    s.start(5, 0);
    s.update(1, 2);
    s.finish();
    let t = extract_text(&rows, rect(0, 0, 80, 3), &s);
    assert_eq!(t, "f\nxyz\npq");
}

#[test]
fn test_extract_skips_spinner_row() {
    // selection spans a spinner row at the tail; the spinner text is
    // excluded from the copied text.
    let rows = vec![row("above"), srow("⠋ thinking (5s)"), row("below")];
    let mut s = Selection::default();
    s.start(0, 0);
    s.update(4, 2);
    s.finish();
    let t = extract_text(&rows, rect(0, 0, 80, 3), &s);
    assert_eq!(t, "above\nbelow");
}

// A structured-diff content row carries a line-number + sigil gutter that
// must NOT be copied (the gutter is wrapped in a no-select span so a
// fullscreen drag yields clean code), and the inter-hunk "..." gap row is
// dropped entirely. A drag across an add + gap + context row copies the
// two content lines with no gutter and no gap.
#[test]
fn test_copy_diff_strips_gutter() {
    let rows: Vec<(u8, String)> = vec![
        (TAG_DIFF_ADD, "+ 3 let x = 1;".to_string()),
        (TAG_DIFF_HUNK, "...".to_string()),
        (TAG_DIFF_CTX, "  2 fn foo() {".to_string()),
    ];
    let mut s = Selection::default();
    s.start(0, 0);
    s.update(20, 2);
    s.finish();
    let t = extract_text(&rows, rect(0, 0, 80, 3), &s);
    assert_eq!(t, "let x = 1;\nfn foo() {");
}

#[test]
fn test_word_bounds_middle_token() {
    assert_eq!(word_bounds_at("hello world", 0, 80), (0, 5));
    assert_eq!(word_bounds_at("hello world", 4, 80), (0, 5));
    assert_eq!(word_bounds_at("hello world", 6, 80), (6, 11));
    // whitespace click → the interior whitespace run (the word-bounds semantics)
    assert_eq!(word_bounds_at("hello world", 5, 80), (5, 6));
}

#[test]
fn test_word_bounds_blank_tail() {
    // A click beyond the text end selects the WHOLE blank tail from the
    // text end to the viewport width in one click — never the last word.
    assert_eq!(word_bounds_at("hello", 10, 20), (5, 20));
    // Trailing spaces in the text join the blank-tail run.
    assert_eq!(word_bounds_at("hi   ", 9, 20), (2, 20));
    // Empty row: the whole blank line.
    assert_eq!(word_bounds_at("", 7, 20), (0, 20));
}

#[test]
fn test_word_bounds_path_selects() {
    // path-friendly punctuation is word-class, so the whole path is one word
    assert_eq!(word_bounds_at("~/.claude/config.json", 2, 80), (0, 21));
}

#[test]
fn test_cjk_word_single_glyph() {
    // Double-click on a CJK glyph selects just that glyph (2 display
    // cols), not the whole contiguous CJK run.
    assert_eq!(word_bounds_at("你好吗", 0, 80), (0, 2));
    assert_eq!(word_bounds_at("你好吗", 2, 80), (2, 4));
    assert_eq!(word_bounds_at("你好吗", 3, 80), (2, 4));
    // ASCII words embedded next to CJK still select as runs ("Zig" spans
    // display cols 3..6 after the wide glyph and the space).
    assert_eq!(word_bounds_at("用 Zig 写", 4, 80), (3, 6));
}

#[test]
fn test_blank_anchor_promotion() {
    // Press in the blank tail, then any drag nudge: the whole blank run
    // (text end to viewport width) is selected in one gesture, matching
    // the word-bounds rule — not a cell-by-cell crawl.
    let rows = vec![row("short text")];
    let r = rect(0, 0, 40, 1);
    let mut s = Selection::default();
    s.start(20, 0);
    s.promote_blank_anchor(&rows, r);
    assert!(
        s.span_origin.is_some(),
        "blank anchor must promote to a span"
    );
    s.extend_span(&rows, r, 19, 0); // nudge left, still inside the run
    s.finish();
    // Whole blank tail: cols 10 (text end) .. 39 (right edge).
    assert_eq!(s.bounds(), Some(((10, 0), (39, 0))));
    // An anchor inside the text must NOT promote.
    let mut t = Selection::default();
    t.start(3, 0);
    t.promote_blank_anchor(&rows, r);
    assert!(t.span_origin.is_none());
}

#[test]
fn test_widget_rows_not_selectable() {
    // Widget-painted rows (the /context block) have no row text; word
    // select must refuse them and copy must skip them, so the clipboard
    // never fills with blank lines.
    let rows = vec![
        row("above"),
        (TAG_WIDGET, String::new()),
        (TAG_WIDGET, String::new()),
        row("below"),
    ];
    let r = rect(0, 0, 80, 4);
    let mut s = Selection::default();
    s.select_word(&rows, r, 5, 1);
    assert!(!s.has_selection(), "word select on a widget row is a no-op");
    s.start(0, 0);
    s.update(4, 3);
    s.finish();
    assert_eq!(extract_text(&rows, r, &s), "above\nbelow");
}

#[test]
fn test_bare_press_no_highlight() {
    // The bare-press semantics: a press sets no focus, and a phantom
    // drag event at the anchor cell (trackpad tremor) stays a no-op — a
    // bare click never highlights a cell or produces copyable text. Once
    // a real motion sets the focus, tracking back onto the anchor cell
    // works normally.
    let mut s = Selection::default();
    s.start(5, 2);
    assert!(!s.has_selection());
    s.update(5, 2); // phantom drag at the press cell
    assert!(!s.has_selection(), "tremor at the anchor must not select");
    assert!(s.is_click_only());
    s.update(6, 2); // real motion
    assert!(s.has_selection());
    s.update(5, 2); // back onto the anchor cell still tracks
    assert_eq!(s.focus, Some((5, 2)));
    assert!(s.drag_moved);
}

#[test]
fn test_start_drops_stale_span() {
    // Regression (debug-log confirmed): a line select leaves span_origin
    // set; the NEXT plain press starts a char-mode drag, and the following
    // drag must extend from the new anchor — not teleport to the stale
    // line span rows and select an unrelated block.
    let rows = vec![row("aaa"), row("bbb"), row("ccc")];
    let r = rect(0, 0, 80, 3);
    let mut s = Selection::default();
    s.select_line(r, 2);
    s.finish();
    // Fresh press on row 0, drag one column right.
    s.start(1, 0);
    assert!(s.span_origin.is_none(), "press must drop the stale span");
    s.extend_span(&rows, r, 2, 0); // no-op without a span
    s.update(2, 0);
    s.finish();
    assert_eq!(s.bounds(), Some(((1, 0), (2, 0))));
}

#[test]
fn test_select_word_sets_span() {
    let rows = vec![row("hello world")];
    let mut s = Selection::default();
    s.select_word(&rows, rect(0, 0, 80, 1), 0, 0);
    assert_eq!(s.anchor, Some((0, 0)));
    assert_eq!(s.focus, Some((4, 0))); // 'hello' cols 0..4 inclusive
    assert!(s.span_origin.is_some());
}

#[test]
fn test_select_line_full_width() {
    let mut s = Selection::default();
    s.select_line(rect(0, 0, 10, 1), 0);
    assert_eq!(s.anchor, Some((0, 0)));
    assert_eq!(s.focus, Some((9, 0)));
}

#[test]
fn test_extract_cjk_by_display() {
    // Two CJK chars (2 display cols each). Select cols [2,4) => 2nd char.
    let rows = vec![row("你好")];
    let mut s = Selection::default();
    s.start(2, 0);
    s.update(3, 0);
    s.finish();
    let t = extract_text(&rows, rect(0, 0, 80, 1), &s);
    assert_eq!(t, "好");
}

#[test]
fn test_extract_full_rows_scrolled() {
    // Content rows are absolute indices into the full row set, so a
    // selection made while the viewport is scrolled needs no offset math
    // at extract time.
    let all_rows: Vec<(u8, String)> = (0..10).map(|i| row(&format!("line{i}"))).collect();
    let mut s = Selection::default();
    s.start(0, 5);
    s.update(79, 7);
    s.finish();
    let t = extract_text(&all_rows, rect(0, 0, 80, 5), &s);
    assert_eq!(t, "line5\nline6\nline7");
}

#[test]
fn test_extract_full_rows_clamped() {
    // Selection extends past the last row; missing rows are silently
    // skipped (no panic, no placeholder blank lines).
    let all_rows = vec![row("a"), row("b")];
    let mut s = Selection::default();
    s.start(0, 0);
    s.update(79, 5);
    s.finish();
    let t = extract_text(&all_rows, rect(0, 0, 80, 6), &s);
    assert_eq!(t, "a\nb");
}

#[test]
fn test_back_drag_keeps_word() {
    // Double-click "world" (cols 6..10), then drag left onto "hello".
    // The original word must stay selected: the range runs from the start
    // of "hello" to the END of "world" (the extend-selection
    // swaps the anchor to the far span edge when extending backward).
    let rows = vec![row("hello world")];
    let r = rect(0, 0, 80, 1);
    let mut s = Selection::default();
    s.select_word(&rows, r, 7, 0);
    assert_eq!(s.bounds(), Some(((6, 0), (10, 0))));
    s.extend_span(&rows, r, 1, 0);
    s.finish();
    let t = extract_text(&rows, r, &s);
    assert_eq!(t, "hello world");
}

#[test]
fn test_line_drag_extends_lines() {
    // Triple-click line 0 then drag down to line 1: both rows fully
    // selected (line mode extends line-by-line, not word-by-word).
    let rows = vec![row("first line"), row("second line")];
    let r = rect(0, 0, 20, 2);
    let mut s = Selection::default();
    s.select_line(r, 0);
    s.extend_span(&rows, r, 3, 1);
    s.finish();
    let t = extract_text(&rows, r, &s);
    assert_eq!(t, "first line\nsecond line");
}

#[test]
fn test_blank_drag_is_symmetric() {
    // Press in the blank area right of the text, drag left vs right by the
    // same distance: both are single-row char-mode selections, reflections
    // of each other — never multi-row, never asymmetric.
    let rows = vec![row("short"), row("short")];
    let r = rect(0, 0, 80, 2);
    let mut left = Selection::default();
    left.start(40, 0);
    left.update(30, 0);
    left.finish();
    let mut right = Selection::default();
    right.start(40, 0);
    right.update(50, 0);
    right.finish();
    assert_eq!(left.bounds(), Some(((30, 0), (40, 0))));
    assert_eq!(right.bounds(), Some(((40, 0), (50, 0))));
    // Both extract the same text (the blank tail holds nothing).
    assert_eq!(extract_text(&rows, r, &left), "");
    assert_eq!(extract_text(&rows, r, &right), "");
}

#[test]
fn test_drag_scroll_extends() {
    // Regression: the prior shift_anchor_row approach moved the anchor
    // toward the focus on edge-scroll, shrinking the selection. With
    // content-space endpoints, an edge-scroll only changes what maps to
    // the viewport; the anchor stays at its content row and the focus
    // extends forward, so the selection GROWS (never shrinks).
    let all_rows: Vec<(u8, String)> = (0..10).map(|i| row(&format!("r{i}"))).collect();
    let r = rect(0, 0, 80, 5);
    let mut s = Selection::default();
    s.start(0, 0); // anchor content row 0
    // user drags to the bottom edge; viewport scrolled down 4 so the edge
    // screen row maps to content row 8:
    s.update(79, 8);
    s.finish();
    let t = extract_text(&all_rows, r, &s);
    // content rows 0..=8 inclusive
    assert_eq!(t, "r0\nr1\nr2\nr3\nr4\nr5\nr6\nr7\nr8");
}
