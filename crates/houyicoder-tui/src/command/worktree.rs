//! /worktrees pane-action methods on App. Extracted from command.rs so that
//! file stays under the file-size gate. The methods are the in-TUI surface:
//! the list refresh on pane-open, the cursor up/down, and the Enter (enter
//! the selected worktree) + d (remove it) actions. Enter and d route through
//! spawn_run — the agent worktree tools are model-invoked (EnterWorktree
//! is model-invoked the same way), so the pane composes a clear user
//! instruction and the model calls the tool. The remove path still hits the
//! approval gate because exit_worktree(remove) is registered to require it.

use crate::state::App;

impl App {
    /// Refresh the worktree list from git worktree list --porcelain against
    /// the project working directory. Called on /worktrees pane-open. Pure
    /// client state — a host-side git spawn fills worktree_entries; the next
    /// render shows the rows. Resets the cursor so it never points past the
    /// new list. No-op when no working directory is set.
    pub fn refresh_worktrees(&mut self) {
        if self.working_dir.is_empty() {
            self.worktree_entries.clear();
            self.worktree_list.cursor = 0;
            return;
        }
        let entries =
            crate::composition::parse_worktrees(Some(&self.working_dir), &self.working_dir);
        self.worktree_list.clamp(entries.len());
        self.worktree_entries = entries;
    }

    /// Move the /worktrees pane cursor up/down. When search is active, the
    /// cursor jumps to the next/prev item in the filtered subset (skipping
    /// filtered-out items) so Up/Down never lands on an invisible row. The
    /// cursor is always an index into worktree_entries, so Enter/d act on
    /// the visible item the cursor points at.
    pub fn move_worktree_cursor(&mut self, delta: i32) {
        if !self.worktree_list.searching() {
            self.worktree_list
                .move_cursor(delta, self.worktree_entries.len());
            return;
        }
        let filtered = crate::list_pane_state::filter_by_query(
            &self.worktree_entries,
            &self.worktree_list.query,
            |e: &crate::composition::WorktreeEntry| vec![&e.path, &e.branch],
        );
        if filtered.is_empty() {
            return;
        }
        let cur = self.worktree_list.cursor.min(self.worktree_entries.len());
        let pos = filtered.iter().position(|&i| i == cur).unwrap_or(0) as i32;
        let next = (pos + delta).clamp(0, (filtered.len() - 1) as i32) as usize;
        self.worktree_list.cursor = filtered[next];
    }

    /// Enter the worktree under the cursor (the Enter action). Composes a
    /// user instruction naming the worktree path and its slug, then starts a
    /// new user turn — the model calls the enter_worktree tool. No-op when no
    /// carrier or the list is empty.
    pub fn enter_worktree_at_cursor(&mut self) {
        let Some(entry) = self
            .worktree_entries
            .get(self.worktree_list.cursor)
            .cloned()
        else {
            return;
        };
        if self.session.is_none() {
            self.system_line("worktree: no carrier (stub mode)".to_string());
            return;
        }
        let slug = std::path::Path::new(&entry.path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.path.clone());
        let msg = format!(
            "Enter the worktree at {path} (call the enter_worktree tool with name {slug}).",
            path = entry.path,
            slug = slug,
        );
        self.system_line(format!("worktree: entering {slug}..."));
        self.spawn_run(msg);
    }
}

#[cfg(test)]
mod tests {
    use crate::composition::{WorktreeEntry, app};

    fn row(path: &str) -> WorktreeEntry {
        WorktreeEntry {
            path: path.into(),
            head: "abcdef0".into(),
            branch: "main".into(),
            is_current: false,
        }
    }

    /// The cursor clamps to the list: moving past the last row holds at the
    /// last, moving above the first holds at the first. No out-of-bounds
    /// index for the Enter / d actions to dereference.
    #[test]
    fn test_worktree_cursor_clamps() {
        let mut app = app();
        app.worktree_entries = vec![row("/a"), row("/b"), row("/c")];
        app.move_worktree_cursor(1);
        assert_eq!(app.worktree_list.cursor, 1, "down one");
        app.move_worktree_cursor(100);
        assert_eq!(
            app.worktree_list.cursor, 2,
            "past the end clamps to the last row"
        );
        app.move_worktree_cursor(-100);
        assert_eq!(
            app.worktree_list.cursor, 0,
            "past the start clamps to the first row"
        );
    }

    /// An empty list is a no-op (no panic, cursor stays at zero) so opening
    /// the pane in a non-repo directory does not index into nothing.
    #[test]
    fn test_worktree_cursor_noop_empty() {
        let mut app = app();
        app.worktree_entries = Vec::new();
        app.move_worktree_cursor(1);
        assert_eq!(
            app.worktree_list.cursor, 0,
            "empty list keeps cursor at zero"
        );
    }

    /// refresh_worktrees with no working directory clears the list + resets
    /// the cursor, so the pane renders the fallback message rather than stale
    /// rows from a previous repo.
    #[test]
    fn test_refresh_clears_without_dir() {
        let mut app = app();
        app.worktree_entries = vec![row("/stale")];
        app.worktree_list.cursor = 0;
        app.working_dir = String::new();
        app.refresh_worktrees();
        assert!(app.worktree_entries.is_empty(), "list cleared");
    }

    #[test]
    fn test_enter_noop_empty() {
        let mut app = app();
        app.worktree_entries = Vec::new();
        app.enter_worktree_at_cursor();
    }

    #[test]
    fn test_cursor_search_skips_filtered() {
        let mut app = app();
        app.worktree_entries = vec![row("/alpha"), row("/beta"), row("/gamma")];
        app.worktree_list.query = "beta".into();
        app.move_worktree_cursor(1);
        // Filtered has 1 item; cursor jumps to it.
        assert_eq!(
            app.worktree_list.cursor, 1,
            "search cursor lands on the filtered item"
        );
    }

    #[test]
    fn test_cursor_search_empty_noop() {
        let mut app = app();
        app.worktree_entries = vec![row("/a"), row("/b")];
        app.worktree_list.query = "zzz".into();
        app.move_worktree_cursor(1);
        // No filtered items: cursor stays put.
        assert_eq!(
            app.worktree_list.cursor, 0,
            "empty search results keep cursor"
        );
    }

    #[test]
    fn test_enter_no_carrier() {
        let mut app = app();
        app.worktree_entries = vec![row("/some/wt")];
        app.worktree_list.cursor = 0;
        // No session wired (stub mode): the enter path reports no carrier.
        app.enter_worktree_at_cursor();
        assert!(
            app.transcript
                .iter()
                .any(|l| matches!(l, crate::records::TranscriptLine::System(s) if s.contains("no carrier"))),
            "stub mode should report no carrier"
        );
    }
}
