//! Worktree pane state transitions and actions: the list refresh on
//! pane-open, cursor movement, and the Enter (enter worktree) + d (remove)
//! actions. Both route through spawn_run — the agent worktree tools are
//! model-invoked, so the pane composes the instruction and the model calls
//! the tool. The remove path still hits the approval gate.

use crate::composition::WorktreeEntry;
use crate::composition::parse_worktrees;
use crate::list_pane_state::filter_by_query;
use crate::state::App;
use std::path::Path;

impl App {
    /// Refresh the worktree list from git worktree list --porcelain against
    /// the project working directory. Called on /worktrees pane-open. Pure
    /// client state — a host-side git listing fills the session's worktree
    /// rows; the next render shows them. Resets the cursor so it never
    /// points past the new list. No-op when no working directory is set.
    pub fn refresh_worktrees(&mut self) {
        if self.sessions.current.identity.working_dir.is_empty() {
            self.sessions.current.capabilities.worktree.entries.clear();
            self.ui.worktree_list.cursor = 0;
            return;
        }
        let entries = parse_worktrees(
            Some(&self.sessions.current.identity.working_dir),
            &self.sessions.current.identity.working_dir,
        );
        self.ui.worktree_list.clamp(entries.len());
        self.sessions.current.capabilities.worktree.entries = entries;
    }

    /// Move the /worktrees pane cursor up/down. When search is active, the
    /// cursor jumps to the next/prev item in the filtered subset (skipping
    /// filtered-out items) so Up/Down never lands on an invisible row. The
    /// cursor is always an index into the session's worktree rows, so
    /// Enter/d act on the visible item the cursor points at.
    pub fn move_worktree_cursor(&mut self, delta: i32) {
        let entries = &self.sessions.current.capabilities.worktree.entries;
        if !self.ui.worktree_list.searching() {
            self.ui.worktree_list.move_cursor(delta, entries.len());
            return;
        }
        let filtered = filter_by_query(
            entries,
            &self.ui.worktree_list.query,
            |e: &WorktreeEntry| vec![&e.path, &e.branch],
        );
        if filtered.is_empty() {
            return;
        }
        let cur = self.ui.worktree_list.cursor.min(entries.len());
        let pos = filtered.iter().position(|&i| i == cur).unwrap_or(0) as i32;
        let next = (pos + delta).clamp(0, (filtered.len() - 1) as i32) as usize;
        self.ui.worktree_list.cursor = filtered[next];
    }

    /// Enter the worktree under the cursor (the Enter action). Composes a
    /// user instruction naming the worktree path and its slug, then starts a
    /// new user turn — the model calls the enter_worktree tool. No-op when no
    /// carrier or the list is empty.
    pub fn enter_worktree_at_cursor(&mut self) {
        let entries = &self.sessions.current.capabilities.worktree.entries;
        let Some(entry) = entries.get(self.ui.worktree_list.cursor).cloned() else {
            return;
        };
        if self.sessions.current.connection.is_none() {
            self.system_line("worktree: not connected".to_string());
            return;
        }
        let slug = Path::new(&entry.path)
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
mod tests;
