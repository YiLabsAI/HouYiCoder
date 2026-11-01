//! Text a selection covers, and the word boundaries a multi-click snaps to.
//!
//! Rows carry display text only, so every lookup walks graphemes and
//! accumulates display width. A wide glyph counts two columns, and a word
//! boundary is a change of grapheme class rather than whitespace alone, so
//! path-like tokens select whole.

use ratatui::layout::Rect;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    Selection, TAG_AGENT_FIRST, TAG_DIFF_ADD, TAG_DIFF_CTX, TAG_DIFF_DEL, diff_gutter_skip,
    is_non_selectable,
};

/// Extract the selected text from a transcript row set indexed by content
/// row. Cells are addressed by display column (CJK width 2), so the
/// substring is sliced by accumulated grapheme width, not byte offset. The
/// un-normalized anchor/focus keep each endpoint's column tied to its own
/// row (correct for all four drag directions). Spinner and fold rows are
/// skipped so the live indicator and collapsed-group summaries never
/// pollute the copied text.
pub fn extract_text(rows: &[(u8, String)], rect: Rect, sel: &Selection) -> String {
    let Some(((start_col, start_row), (end_col, end_row))) = sel.bounds() else {
        return String::new();
    };
    let mut out = String::new();
    let mut wrote = false;
    for ri in start_row..=end_row {
        let Some((tag, row)) = rows.get(ri) else {
            continue;
        };
        if is_non_selectable(*tag) {
            continue;
        }
        let row_w = UnicodeWidthStr::width(row.as_str());
        let (col_start, col_end) = if start_row == end_row {
            let c0 = start_col.min(end_col).saturating_sub(rect.x) as usize;
            let c1 = start_col.max(end_col).saturating_sub(rect.x) as usize + 1;
            (c0, c1)
        } else if ri == start_row {
            (start_col.saturating_sub(rect.x) as usize, row_w)
        } else if ri == end_row {
            (0, end_col.saturating_sub(rect.x) as usize + 1)
        } else {
            (0, row_w)
        };
        if wrote {
            out.push('\n');
        }
        let glyph_skip = if *tag == TAG_AGENT_FIRST {
            2
        } else if matches!(*tag, TAG_DIFF_ADD | TAG_DIFF_DEL | TAG_DIFF_CTX) {
            diff_gutter_skip(row)
        } else {
            0
        };
        let col_start = col_start.max(glyph_skip);
        out.push_str(&substring_by_display(row, col_start, col_end));
        wrote = true;
    }
    out
}

/// Slice a string by display columns [c0, c1): walk graphemes, accumulate
/// display width, keep graphemes that fall in the range.
fn substring_by_display(s: &str, c0: usize, c1: usize) -> String {
    let mut w = 0usize;
    let mut out = String::new();
    for g in s.graphemes(true) {
        if w >= c1 {
            break;
        }
        let gw = UnicodeWidthStr::width(g);
        if w + gw > c0 {
            out.push_str(g);
        }
        w += gw;
    }
    out
}

/// 3-class grapheme classifier (space / word / other-punct) — a word boundary
/// is a class change. Word chars include path-friendly punctuation so a token
/// like ~/.claude/config.json selects whole.
fn char_class(g: &str) -> u8 {
    if g.chars().all(|c| c.is_whitespace()) {
        0
    } else if g.chars().any(|c| {
        c.is_alphanumeric()
            || c == '_'
            || c == '/'
            || c == '.'
            || c == '-'
            || c == '+'
            || c == '~'
            || c == '\\'
    }) {
        1
    } else {
        2
    }
}

/// The display-column bounds [lo, hi) of the same-class grapheme run at
/// the column — a word-bounds lookup: word chars, punctuation
/// AND whitespace each form a run, so a double-click on spaces selects the
/// whitespace run (not a single cell) and drags extend symmetrically in
/// both directions. A column beyond the text end selects the WHOLE blank
/// tail out to max_w (the viewport width) in one click — the screen
/// buffer holds real blank cells there, so a word select grabs
/// the entire stretch; never snapping back onto the last word.
pub(crate) fn word_bounds_at(row: &str, col: usize, max_w: usize) -> (usize, usize) {
    let graphemes: Vec<&str> = row.graphemes(true).collect();
    // display-col start of each grapheme.
    let mut starts = Vec::with_capacity(graphemes.len());
    let mut widths = Vec::with_capacity(graphemes.len());
    let mut w = 0usize;
    for g in &graphemes {
        starts.push(w);
        let gw = UnicodeWidthStr::width(*g);
        widths.push(gw);
        w += gw;
    }
    // Beyond the text end (or an empty row): the blank tail is one
    // whitespace run from where the text stops out to the viewport width.
    // The trailing-space run of the text joins it (same class).
    if col >= w {
        let mut lo = w;
        let mut i = graphemes.len();
        while i > 0 && char_class(graphemes[i - 1]) == 0 {
            i -= 1;
            lo = starts[i];
        }
        return (lo, max_w.max(col + 1));
    }
    // index of the grapheme covering col (start <= col < start + width)
    let idx = (0..graphemes.len())
        .find(|&i| starts[i] + widths[i] > col)
        .unwrap_or(graphemes.len() - 1);
    // Wide glyphs (CJK) are each their own word: a double-click selects
    // the single character — CJK has no space-
    // delimited word boundaries, so run expansion grabbed whole phrases.
    if widths[idx] >= 2 {
        return (starts[idx], starts[idx] + widths[idx]);
    }
    let cls = char_class(graphemes[idx]);
    let mut lo = idx;
    while lo > 0 && char_class(graphemes[lo - 1]) == cls {
        lo -= 1;
    }
    let mut hi = idx;
    while hi + 1 < graphemes.len() && char_class(graphemes[hi + 1]) == cls {
        hi += 1;
    }
    // A whitespace run at the text end extends through the blank tail
    // (the screen buffer has real blank cells there; our row
    // strings stop at the text, so extend the run to the click column).
    (starts[lo], starts[hi] + widths[hi])
}
