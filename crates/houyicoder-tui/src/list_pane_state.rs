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
mod tests;
