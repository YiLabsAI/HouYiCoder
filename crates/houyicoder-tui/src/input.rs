//! Multi-line input editor backing the chat input box. Cursor-based with
//! grapheme-aligned movement (unicode-segmentation) and display-width-aware
//! hard wrapping (unicode-width) so CJK and combining marks lay out
//! correctly. The logical text is one String with newlines; the cursor is a
//! byte offset. Wrapping is derived on demand from a column width, never
//! stored — the same derivation drives the rendered height and the
//! cursor's wrapped-line position, so the two never drift.
//!
//! API kept compatible with the old append-only field: push inserts at the
//! cursor, pop backspaces the grapheme before the cursor. New methods cover
//! mid-string editing and movement. The view layer reads wrapped_lines /
//! line_count to size the input box and render wrapped content.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// One visual row of the wrapped input. start_offset is the byte offset into
/// the full text where this row's content begins, so a (row, column) screen
/// position maps back to a byte offset for click-to-position (future).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedLine {
    pub text: String,
    pub start_offset: usize,
}

/// A cursor-aware, grapheme-aligned, hard-wrapping text editor.
#[derive(Debug, Default, Clone)]
pub struct InputField {
    text: String,
    cursor: usize,
}

/// Cap so a huge paste never eats the whole screen: the input box tops out
/// near half the terminal height (minus the status row that sits below it).
const MIN_CONTENT_LINES: usize = 1;

impl InputField {
    /// Create an empty field.
    pub fn new() -> Self {
        Self::default()
    }

    /// The full logical text (newlines included).
    pub fn value(&self) -> &str {
        &self.text
    }

    /// True when nothing is typed.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Current cursor byte offset (grapheme-aligned after any move).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Insert a string at the cursor; cursor advances past it.
    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Insert one char at the cursor. Kept as the push() entry point so
    /// existing call sites (typing) gain cursor-awareness for free: when the
    /// cursor is at the end this is identical to append; mid-string it inserts
    /// in place.
    pub fn push(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.insert_str(s);
    }

    /// Backspace the grapheme immediately before the cursor. No-op at offset 0.
    pub fn pop(&mut self) {
        self.backspace();
    }

    /// Backspace the grapheme before the cursor.
    pub fn backspace(&mut self) {
        let Some((start, _)) = self.text[..self.cursor].grapheme_indices(true).next_back() else {
            return;
        };
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Delete the grapheme at/after the cursor. No-op past the end.
    pub fn delete(&mut self) {
        let mut next = self.text[self.cursor..].grapheme_indices(true);
        let Some((rel, g)) = next.next() else {
            return;
        };
        let _ = next;
        let end = self.cursor + rel + g.len();
        self.text.drain(self.cursor..end);
    }

    /// Replace the buffer; cursor lands at the end (typical for set/restore).
    pub fn set(&mut self, s: String) {
        self.text = s;
        self.cursor = self.text.len();
    }

    /// Reset to empty.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Take ownership of the text, leaving an empty field.
    pub fn take(&mut self) -> String {
        let t = std::mem::take(&mut self.text);
        self.cursor = 0;
        t
    }

    /// Kill the entire logical line the cursor is on (between newlines, or to
    /// the text end), including the trailing newline so the lines below
    /// collapse up. Cursor lands at the start of where the line was.
    pub fn kill_current_line(&mut self) {
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let line_end = self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len());
        let delete_end = if line_end < self.text.len() {
            line_end + 1
        } else {
            line_end
        };
        self.text.drain(line_start..delete_end);
        self.cursor = line_start;
    }

    /// Kill from the start of the wrapped line the cursor is on, up to the
    /// cursor (readline Ctrl+U: delete backward to the line start, not the
    /// whole buffer). No-op when the cursor is already at the line start.
    pub fn kill_to_line_start(&mut self, cols: usize) {
        let (idx, _) = self.cursor_position(cols);
        let lines = self.wrapped_lines(cols);
        let Some(wl) = lines.get(idx) else {
            return;
        };
        let start = wl.start_offset;
        if start < self.cursor {
            self.text.drain(start..self.cursor);
            self.cursor = start;
        }
    }

    /// Move the cursor one grapheme left. No-op at 0.
    pub fn move_left(&mut self) {
        if let Some((i, _)) = self.text[..self.cursor].grapheme_indices(true).next_back() {
            self.cursor = i;
        }
    }

    /// Move the cursor one grapheme right. No-op past the end.
    pub fn move_right(&mut self) {
        if let Some((rel, g)) = self.text[self.cursor..].grapheme_indices(true).next() {
            self.cursor += rel + g.len();
        }
    }

    /// Move the cursor to the start of the whole text.
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end of the whole text.
    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Set the cursor to a byte offset, clamped to the text bounds. The caller
    /// must pass a char boundary (the queue-recall merge passes a newline
    /// boundary); a mid-grapheme offset would split a multi-byte char, so the
    /// assert is enforced in debug builds.
    pub fn move_to(&mut self, byte_pos: usize) {
        let pos = byte_pos.min(self.text.len());
        debug_assert!(
            self.text.is_char_boundary(pos),
            "move_to offset must be a char boundary"
        );
        self.cursor = pos;
    }

    /// Move the cursor to the start of the wrapped visual line it is on.
    /// Readline-style double-press: if the cursor is already at column 0 of
    /// this visual line and there is a visual line above, jump to that one's
    /// start so a second press walks up the wrapped lines.
    pub fn move_line_home(&mut self, cols: usize) {
        let (idx, col) = self.cursor_position(cols);
        let lines = self.wrapped_lines(cols);
        if col == 0 && idx > 0 {
            if let Some(prev) = lines.get(idx - 1) {
                self.cursor = prev.start_offset;
            }
        } else if let Some(wl) = lines.get(idx) {
            self.cursor = wl.start_offset;
        }
    }

    /// Move the cursor to the end of the wrapped line it is on.
    pub fn move_line_end(&mut self, cols: usize) {
        let pos = self.cursor_position(cols);
        if let Some(wl) = self.wrapped_lines(cols).get(pos.0) {
            self.cursor = wl.start_offset + wl.text.len();
        }
    }

    /// Move the cursor one wrapped line up, preserving the column. No-op on
    /// the first wrapped line.
    pub fn move_up(&mut self, cols: usize) {
        let (idx, col) = self.cursor_position(cols);
        if idx == 0 {
            return;
        }
        let lines = self.wrapped_lines(cols);
        let target = &lines[idx - 1];
        self.cursor = self.byte_at_column(target, col);
    }

    /// Move the cursor one wrapped line down, preserving the column. No-op on
    /// the last wrapped line.
    pub fn move_down(&mut self, cols: usize) {
        let (idx, col) = self.cursor_position(cols);
        let lines = self.wrapped_lines(cols);
        if idx + 1 >= lines.len() {
            return;
        }
        let target = &lines[idx + 1];
        self.cursor = self.byte_at_column(target, col);
    }

    /// Hard-wrap the text to the given column count (display cells). Each
    /// logical line (newline-separated) wraps independently; an empty logical
    /// line yields one empty wrapped row. A grapheme wider than the column
    /// overflows rather than splitting (graphemes are atomic).
    pub fn wrapped_lines(&self, columns: usize) -> Vec<WrappedLine> {
        if columns == 0 {
            return vec![WrappedLine {
                text: self.text.clone(),
                start_offset: 0,
            }];
        }
        let mut out: Vec<WrappedLine> = Vec::new();
        let mut offset = 0usize;
        for logical in self.text.split('\n') {
            if logical.is_empty() {
                out.push(WrappedLine {
                    text: String::new(),
                    start_offset: offset,
                });
            } else {
                let mut line = String::new();
                let mut line_off = offset;
                let mut w = 0usize;
                for (g_rel, g) in logical.grapheme_indices(true) {
                    let gw = UnicodeWidthStr::width(g);
                    if !line.is_empty() && w + gw > columns {
                        out.push(WrappedLine {
                            text: std::mem::take(&mut line),
                            start_offset: line_off,
                        });
                        line_off = offset + g_rel;
                        w = 0;
                    }
                    line.push_str(g);
                    w += gw;
                }
                out.push(WrappedLine {
                    text: line,
                    start_offset: line_off,
                });
            }
            offset += logical.len() + 1; // +1 for the '\n'
        }
        if out.is_empty() {
            out.push(WrappedLine {
                text: String::new(),
                start_offset: 0,
            });
        }
        out
    }

    /// Number of wrapped rows at this column width (>= 1).
    pub fn line_count(&self, columns: usize) -> usize {
        self.wrapped_lines(columns).len().max(1)
    }

    /// Rendered input height in terminal rows: wrapped content rows capped to
    /// roughly half the terminal (minus the status row), plus 2 for the top
    /// and bottom border lines.
    pub fn input_height(&self, total_rows: u16, cols: usize) -> u16 {
        let half = (total_rows / 2)
            .saturating_sub(1)
            .max(MIN_CONTENT_LINES as u16) as usize;
        let content = self.line_count(cols).min(half).max(MIN_CONTENT_LINES);
        content as u16 + 2
    }

    /// The (wrapped-line index, display column) of the cursor. A cursor
    /// exactly at a wrap boundary (end of row i == start of row i+1, with no
    /// newline between them) belongs to the NEXT row, so down-movement past a
    /// wrap point progresses instead of sticking. A cursor at the end of the
    /// last row stays on that row.
    pub fn cursor_position(&self, cols: usize) -> (usize, usize) {
        let lines = self.wrapped_lines(cols);
        for (i, wl) in lines.iter().enumerate() {
            let end = wl.start_offset + wl.text.len();
            let next_starts_here = i + 1 < lines.len() && lines[i + 1].start_offset == self.cursor;
            if self.cursor < end || (self.cursor == end && !next_starts_here) {
                let rel = self
                    .cursor
                    .saturating_sub(wl.start_offset)
                    .min(wl.text.len());
                let col = UnicodeWidthStr::width(&wl.text[..rel]);
                return (i, col);
            }
        }
        (lines.len().saturating_sub(1), 0)
    }

    /// Byte offset of the grapheme boundary nearest the column on this
    /// wrapped line, clamped to the line start..end.
    fn byte_at_column(&self, wl: &WrappedLine, col: usize) -> usize {
        let mut w = 0usize;
        let mut best = wl.start_offset;
        for (g_rel, g) in wl.text.grapheme_indices(true) {
            if w >= col {
                break;
            }
            best = wl.start_offset + g_rel;
            w += UnicodeWidthStr::width(g);
        }
        best.max(wl.start_offset)
            .min(wl.start_offset + wl.text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_pop_clear() {
        let mut f = InputField::new();
        assert!(f.is_empty());
        f.push('a');
        f.push('b');
        assert_eq!("ab", f.value());
        f.pop();
        assert_eq!("a", f.value());
        let taken = f.take();
        assert_eq!("a", taken);
        assert!(f.is_empty());
    }

    #[test]
    fn test_insert_and_backspace_mid() {
        let mut f = InputField::new();
        f.set("hello".to_string());
        f.move_home();
        f.move_right();
        f.move_right();
        f.insert_str("X");
        assert_eq!("heXllo", f.value());
        f.backspace();
        assert_eq!("hello", f.value());
    }

    #[test]
    fn test_cursor_moves_grapheme_aligned() {
        let mut f = InputField::new();
        f.set("a🚀b".to_string()); // graphemes: a, 🚀, b
        f.move_home();
        f.move_right();
        assert_eq!(f.cursor(), 1); // after 'a'
        f.move_right();
        assert_eq!(f.cursor(), "a🚀".len()); // after 🚀, not mid-codepoint
        f.move_left();
        assert_eq!(f.cursor(), 1);
    }

    #[test]
    fn test_wrap_ascii_by_columns() {
        let mut f = InputField::new();
        f.set("hello world".to_string());
        let lines = f.wrapped_lines(5);
        // "hello" / " worl" / "d"
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "hello");
        assert_eq!(lines[1].text, " worl");
        assert_eq!(lines[2].text, "d");
        assert_eq!(f.line_count(5), 3);
    }

    #[test]
    fn test_wrap_cjk_double_width() {
        let mut f = InputField::new();
        f.set("你好世界".to_string()); // each char width 2
        let lines = f.wrapped_lines(4); // 2 CJK per row
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "你好");
        assert_eq!(lines[1].text, "世界");
    }

    #[test]
    fn test_wrap_preserves_newline_rows() {
        let mut f = InputField::new();
        f.set("a\n\nb".to_string());
        let lines = f.wrapped_lines(80);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "a");
        assert_eq!(lines[1].text, "");
        assert_eq!(lines[2].text, "b");
    }

    #[test]
    fn test_input_height_capped() {
        let mut f = InputField::new();
        assert_eq!(f.input_height(24, 80), 3); // 1 content + 2 border
        f.set("a".repeat(200)); // wraps to many lines at 80 cols
        let h = f.input_height(24, 80);
        assert!(h <= 24 / 2 + 2); // capped near half terminal
        assert!(h > 3);
    }

    #[test]
    fn test_kill_current_line_middle() {
        let mut f = InputField::new();
        f.set("line one\nline two\nline three".to_string());
        // cursor at end (after "three") — kill the last line.
        f.kill_current_line();
        assert_eq!(f.value(), "line one\nline two\n");
        // cursor at the trailing newline position.
        // Now kill the middle line: move cursor onto "line two".
        f.set("line one\nline two\nline three".to_string());
        // place cursor at start of "line two" (offset 9)
        f.cursor = "line one\n".len();
        f.kill_current_line();
        assert_eq!(f.value(), "line one\nline three");
    }

    #[test]
    fn test_kill_only_line() {
        let mut f = InputField::new();
        f.set("only line".to_string());
        f.kill_current_line();
        assert!(f.is_empty());
    }

    #[test]
    fn test_move_home_end_whole() {
        let mut f = InputField::new();
        f.set("hello\nworld".to_string());
        f.move_home();
        assert_eq!(f.cursor(), 0);
        f.move_end();
        assert_eq!(f.cursor(), "hello\nworld".len());
    }

    #[test]
    fn test_move_up_down_preserves() {
        let mut f = InputField::new();
        f.set("hello world foo".to_string()); // wraps at 5: hello /  worl / d foo
        f.move_home();
        f.move_down(5);
        assert_eq!(f.cursor_position(5).0, 1);
        f.move_down(5);
        assert_eq!(f.cursor_position(5).0, 2);
        f.move_up(5);
        assert_eq!(f.cursor_position(5).0, 1);
    }
}
