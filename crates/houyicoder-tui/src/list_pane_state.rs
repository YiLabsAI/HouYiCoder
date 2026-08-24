//! Shared cursor + search state for list panes. Each management pane
//! (worktrees, hooks, memory, permissions) owns one of these plus its own
//! entries, row formatter, and key actions. This struct replaces the
//! per-pane cursor field and the copy-pasted move/clamp logic that was
//! duplicated across three panes before this extraction.

/// Cursor position + search query for a list pane. The pane filters its
/// entries against the query, then clamps the cursor to the filtered length
/// before render. Esc clears the query.
#[derive(Default)]
pub(crate) struct ListPaneState {
    pub cursor: usize,
    pub query: String,
}

impl ListPaneState {
    /// Clamp the cursor to the filtered list length. Called after filtering
    /// and before render so the cursor never points past the last visible row.
    pub fn clamp(&mut self, len: usize) {
        self.cursor = self.cursor.min(len.saturating_sub(1));
    }

    /// Move the cursor by a signed delta, clamped to the list bounds. A no-op
    /// on an empty list (no row to point at).
    pub fn move_cursor(&mut self, delta: i32, len: usize) {
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let cur = self.cursor.min(len - 1) as i32;
        self.cursor = (cur + delta).clamp(0, (len - 1) as i32) as usize;
    }

    /// Clear the search query (the Esc action in search mode).
    pub fn clear_query(&mut self) {
        self.query.clear();
    }

    /// Whether the search query is active (non-empty).
    pub fn searching(&self) -> bool {
        !self.query.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clamp_within_bounds() {
        let mut s = ListPaneState {
            cursor: 5,
            ..Default::default()
        };
        s.clamp(10);
        assert_eq!(s.cursor, 5, "cursor within range stays");
    }

    #[test]
    fn test_clamp_past_end() {
        let mut s = ListPaneState {
            cursor: 10,
            ..Default::default()
        };
        s.clamp(3);
        assert_eq!(s.cursor, 2, "cursor past end clamps to last index");
    }

    #[test]
    fn test_clamp_empty() {
        let mut s = ListPaneState {
            cursor: 0,
            ..Default::default()
        };
        s.clamp(0);
        assert_eq!(s.cursor, 0, "empty list keeps cursor at zero");
    }

    #[test]
    fn test_move_cursor_down() {
        let mut s = ListPaneState::default();
        s.move_cursor(1, 5);
        assert_eq!(s.cursor, 1);
        s.move_cursor(1, 5);
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn test_move_cursor_clamps_down() {
        let mut s = ListPaneState {
            cursor: 4,
            ..Default::default()
        };
        s.move_cursor(10, 5);
        assert_eq!(s.cursor, 4, "past the end holds at last row");
    }

    #[test]
    fn test_move_cursor_clamps_up() {
        let mut s = ListPaneState {
            cursor: 0,
            ..Default::default()
        };
        s.move_cursor(-10, 5);
        assert_eq!(s.cursor, 0, "past the start holds at first row");
    }

    #[test]
    fn test_move_cursor_empty_noop() {
        let mut s = ListPaneState {
            cursor: 3,
            ..Default::default()
        };
        s.move_cursor(1, 0);
        assert_eq!(s.cursor, 0, "empty list resets cursor to zero");
    }

    #[test]
    fn test_clear_query() {
        let mut s = ListPaneState {
            query: "test".into(),
            ..Default::default()
        };
        assert!(s.searching());
        s.clear_query();
        assert!(!s.searching());
        assert!(s.query.is_empty());
    }

    #[test]
    fn test_default_no_search() {
        let s = ListPaneState::default();
        assert!(!s.searching());
        assert_eq!(s.cursor, 0);
    }
}
