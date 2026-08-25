//! Shared cursor + search state for list panes. Each management pane
//! (worktrees, hooks, memory, permissions) owns one of these plus its own
//! entries, row formatter, and key actions. This struct replaces the
//! per-pane cursor field and the copy-pasted move/clamp logic that was
//! duplicated across three panes before this extraction.

/// Cursor position + search query for a list pane. The pane filters its
/// entries against the query, then clamps the cursor to the filtered length
/// before render. Esc clears the query.
#[derive(Default)]
pub struct ListPaneState {
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

/// Truncate a path to fit a fixed width, keeping the tail (the most
/// identifying segment) and prefixing with an ellipsis when truncated.
/// Context abbreviations are applied first: HOME -> ~, the worktree
/// root -> ~/wt/, system temp dirs -> <tmp>/. Then if still too long,
/// left-truncate with the ellipsis.
pub fn truncate_path(path: &str, max: usize) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    truncate_path_with_home(path, max, &home)
}

/// Same as truncate_path but with an explicit home directory, so tests
/// do not depend on the ambient HOME (which the instrumented coverage run
/// may not set). The home prefix matches only when it is a directory
/// boundary: HOME itself, or HOME plus a slash.
pub fn truncate_path_with_home(path: &str, max: usize, home: &str) -> String {
    if max == 0 {
        return String::new();
    }
    let abbreviated = if !home.is_empty()
        && path.starts_with(home)
        && (path.len() == home.len() || path.as_bytes()[home.len()] == b'/')
    {
        let rest = &path[home.len()..];
        // Match .claude/worktrees/ anywhere after HOME, not just right after
        // it: linked worktrees live under HOME/<workspace>/.claude/worktrees/,
        // not HOME/.claude/worktrees/. Abbreviate the whole prefix up to + the
        // worktrees segment to ~/wt/, keeping only the tail (the slug).
        if let Some(idx) = rest.find("/.claude/worktrees/") {
            let tail = &rest[idx + "/.claude/worktrees/".len()..];
            format!("~/wt/{tail}")
        } else {
            format!("~{rest}")
        }
    } else if path.starts_with("/private/var/folders/") || path.starts_with("/var/folders/") {
        let basename = std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        format!("<tmp>/{basename}")
    } else {
        path.to_string()
    };
    if abbreviated.chars().count() <= max {
        abbreviated
    } else {
        let chars: Vec<char> = abbreviated.chars().collect();
        let take = max.saturating_sub(1);
        let tail: String = chars[chars.len() - take..].iter().collect();
        format!("\u{2026}{tail}")
    }
}

/// Render the search hint line shown when a query is active. Shared so
/// every list pane shows the same "search: [query] (Esc clears)" shape.
pub fn search_hint_line(query: &str) -> String {
    format!("search: [{query}]  (Esc clears)")
}

/// Filter a list of items by a search query, returning indices into the
/// original slice. Each item is matched by a caller-provided extractor;
/// the query matches if any extracted field contains it (case-insensitive).
/// Empty query returns all indices. Shared so every list pane filters the
/// same way.
pub fn filter_by_query<T, F>(items: &[T], query: &str, extractor: F) -> Vec<usize>
where
    F: Fn(&T) -> Vec<&str>,
{
    if query.is_empty() {
        return (0..items.len()).collect();
    }
    let q = query.to_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            extractor(item)
                .iter()
                .any(|field| field.to_lowercase().contains(&q))
        })
        .map(|(i, _)| i)
        .collect()
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

    #[test]
    fn test_truncate_short() {
        let p = truncate_path("/usr/bin", 32);
        assert_eq!(p, "/usr/bin", "short path unchanged");
    }

    #[test]
    fn test_truncate_truncates_left() {
        let p = truncate_path("/a/b/c/d/e/f/g/h/i/j/k", 10);
        assert!(p.starts_with('\u{2026}'), "long path gets ellipsis prefix");
        assert!(p.chars().count() <= 10, "stays within max width");
        assert!(p.ends_with("k"), "keeps the tail");
    }

    #[test]
    fn test_search_hint_format() {
        let h = search_hint_line("foo");
        assert!(h.contains("foo"), "hint contains the query");
        assert!(h.contains("Esc"), "hint mentions Esc");
    }

    #[test]
    fn test_filter_empty_query() {
        let items = vec!["a", "b", "c"];
        let r = filter_by_query(&items, "", |_| vec![]);
        assert_eq!(r.len(), 3, "empty query returns all");
    }

    #[test]
    fn test_filter_match() {
        let items = vec!["hello", "world", "help"];
        let r = filter_by_query(&items, "hel", |s| vec![s]);
        assert_eq!(r.len(), 2, "hel matches hello + help");
    }

    #[test]
    fn test_filter_case_insensitive() {
        let items = vec!["Hello"];
        let r = filter_by_query(&items, "HEL", |s| vec![s]);
        assert_eq!(r.len(), 1, "case-insensitive match");
    }

    #[test]
    fn test_filter_multi_field() {
        let items = vec![("a", "branch1"), ("b", "branch2")];
        let r = filter_by_query(&items, "branch2", |(_, b)| vec![b]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0], 1);
    }

    #[test]
    fn test_filter_no_match() {
        let items = vec!["a", "b"];
        let r = filter_by_query(&items, "zzz", |s| vec![s]);
        assert!(r.is_empty(), "no match returns empty");
    }

    #[test]
    fn test_truncate_home_prefix() {
        // Synthetic home so the assertion runs regardless of ambient HOME.
        let p = truncate_path_with_home("/home/u/some/repo", 64, "/home/u");
        assert!(p.starts_with("~/"), "HOME prefix becomes ~/");
    }

    #[test]
    fn test_truncate_home_exact() {
        let p = truncate_path_with_home("/home/u", 64, "/home/u");
        assert_eq!(p, "~", "path == HOME becomes ~");
    }

    #[test]
    fn test_truncate_wt_abbrev() {
        let p = truncate_path_with_home("/home/u/.claude/worktrees/feat-x", 64, "/home/u");
        assert!(p.starts_with("~/wt/"), "worktree path abbreviates to ~/wt/");
        assert!(p.contains("feat-x"));
    }

    #[test]
    fn test_truncate_home_boundary() {
        // /home/ubackup must NOT match HOME=/home/u (no slash boundary).
        let p = truncate_path_with_home("/home/ubackup/repo", 64, "/home/u");
        assert!(
            !p.starts_with("~/"),
            "HOME plus suffix without slash must not abbreviate"
        );
    }

    #[test]
    fn test_truncate_max_zero() {
        let p = truncate_path("/anything", 0);
        assert_eq!(p, "", "max=0 returns empty");
    }

    #[test]
    fn test_filter_returns_indices() {
        let items = vec!["a", "bb", "c"];
        let r = filter_by_query(&items, "b", |s| vec![s]);
        assert_eq!(r, vec![1], "returns the index not the value");
    }
}
