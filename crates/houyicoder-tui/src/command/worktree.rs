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
            self.worktree_cursor = 0;
            return;
        }
        let entries =
            crate::composition::parse_worktrees(Some(&self.working_dir), &self.working_dir);
        self.worktree_cursor = self.worktree_cursor.min(entries.len().saturating_sub(1));
        self.worktree_entries = entries;
    }

    /// Move the /worktrees pane cursor one row up/down, clamped to the list.
    /// No-op when the list is empty.
    pub fn move_worktree_cursor(&mut self, delta: i32) {
        let n = self.worktree_entries.len();
        if n == 0 {
            self.worktree_cursor = 0;
            return;
        }
        let cur = self.worktree_cursor.min(n - 1) as i32;
        let next = (cur + delta).clamp(0, (n - 1) as i32) as usize;
        self.worktree_cursor = next;
    }

    /// Enter the worktree under the cursor (the Enter action). Composes a
    /// user instruction naming the worktree path and its slug, then starts a
    /// new user turn — the model calls the enter_worktree tool. No-op when no
    /// carrier or the list is empty.
    pub fn enter_worktree_at_cursor(&mut self) {
        let Some(entry) = self.worktree_entries.get(self.worktree_cursor).cloned() else {
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

    /// Remove the worktree under the cursor (the d action). Composes a user
    /// instruction naming the worktree path, then starts a new user turn —
    /// the model calls exit_worktree with action remove, which the approval
    /// gate intercepts (the tool is registered to require approval for the
    /// remove action) so the user confirms before any deletion. No-op when no
    /// carrier or the list is empty.
    pub fn remove_worktree_at_cursor(&mut self) {
        let Some(entry) = self.worktree_entries.get(self.worktree_cursor).cloned() else {
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
            "Exit the worktree at {path} with action remove \
             (call the exit_worktree tool with action remove).",
            path = entry.path,
        );
        self.system_line(format!(
            "worktree: requesting remove of {slug} (will confirm)..."
        ));
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
        assert_eq!(app.worktree_cursor, 1, "down one");
        app.move_worktree_cursor(100);
        assert_eq!(
            app.worktree_cursor, 2,
            "past the end clamps to the last row"
        );
        app.move_worktree_cursor(-100);
        assert_eq!(
            app.worktree_cursor, 0,
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
        assert_eq!(app.worktree_cursor, 0, "empty list keeps cursor at zero");
    }

    /// refresh_worktrees with no working directory clears the list + resets
    /// the cursor, so the pane renders the fallback message rather than stale
    /// rows from a previous repo.
    #[test]
    fn test_refresh_clears_without_dir() {
        let mut app = app();
        app.worktree_entries = vec![row("/stale")];
        app.worktree_cursor = 0;
        app.working_dir = String::new();
        app.refresh_worktrees();
        assert!(app.worktree_entries.is_empty(), "list cleared");
    }
}
