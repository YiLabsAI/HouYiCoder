//! /worktrees pane key handling. The pane state decides, from its own level,
//! what a key means; this module applies that action to the session. Search is
//! entered with / so the e, d, and b actions stay bare keys on the list.

use crossterm::event::{KeyCode, KeyEvent};

use crate::state::App;
use crate::state::enums::Pane;
use crate::ui::{RemoveFocus, WorktreeLevel, WorktreePaneState};

/// What a key asks the /worktrees pane to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorktreeAction {
    /// Move the list cursor by this many rows.
    Select(i32),
    /// Move the block one worktree level down by this many rows.
    Scroll(i32),
    /// Open the detail for the row under the cursor.
    OpenDetail,
    /// Open the removal confirmation.
    BeginRemoval,
    /// Move the session into the worktree the pane looks at.
    EnterWorktree,
    /// Take the list's keystrokes as a search query.
    StartSearch,
    /// Take one character into the search query.
    Search(char),
    /// Drop the last character of the search query.
    SearchBackspace,
    /// End the search, keeping the query as the filter.
    FinishSearch,
    /// Drop the search query.
    ClearSearch,
    /// Move the confirmation's focus to the other button.
    ToggleFocus,
    /// Act on the focused button of the confirmation.
    Confirm,
    /// Flip whether an unmerged branch goes with the tree.
    ToggleBranch,
    /// Read the worktree again.
    Reread,
    /// Return one level, or close the pane at the list.
    Back,
}

impl WorktreePaneState {
    /// The action a key asks for at the current level, or None when the key is
    /// not this pane's. typing reports that the input box holds text, which
    /// keeps bare letters for the message the user is writing.
    fn action_for(&self, k: KeyEvent, typing: bool) -> Option<WorktreeAction> {
        if self.search.get() {
            return Some(match k.code {
                KeyCode::Char(c)
                    if k.modifiers.is_empty() && (c.is_ascii_graphic() || c == ' ') =>
                {
                    WorktreeAction::Search(c)
                }
                KeyCode::Up => WorktreeAction::Select(-1),
                KeyCode::Down => WorktreeAction::Select(1),
                KeyCode::Backspace => WorktreeAction::SearchBackspace,
                KeyCode::Enter => WorktreeAction::FinishSearch,
                KeyCode::Esc => WorktreeAction::ClearSearch,
                _ => return None,
            });
        }
        match self.level.get() {
            WorktreeLevel::List => self.list_action(k, typing),
            WorktreeLevel::Detail => self.detail_action(k, typing),
            WorktreeLevel::Confirm => self.confirm_action(k, typing),
        }
    }

    fn list_action(&self, k: KeyEvent, typing: bool) -> Option<WorktreeAction> {
        Some(match k.code {
            KeyCode::Up => WorktreeAction::Select(-1),
            KeyCode::Down => WorktreeAction::Select(1),
            KeyCode::Enter if !typing => WorktreeAction::OpenDetail,
            KeyCode::Char('e') if bare(k) && !typing => WorktreeAction::EnterWorktree,
            KeyCode::Char('d') if bare(k) && !typing => WorktreeAction::BeginRemoval,
            KeyCode::Char('/') if bare(k) && !typing => WorktreeAction::StartSearch,
            KeyCode::Esc if self.list.searching() => WorktreeAction::ClearSearch,
            KeyCode::Esc => WorktreeAction::Back,
            _ => return None,
        })
    }

    fn detail_action(&self, k: KeyEvent, typing: bool) -> Option<WorktreeAction> {
        Some(match k.code {
            KeyCode::Up => WorktreeAction::Scroll(-1),
            KeyCode::Down => WorktreeAction::Scroll(1),
            KeyCode::Char('e') if bare(k) && !typing => WorktreeAction::EnterWorktree,
            KeyCode::Char('d') if bare(k) && !typing => WorktreeAction::BeginRemoval,
            KeyCode::Esc => WorktreeAction::Back,
            _ => return None,
        })
    }

    fn confirm_action(&self, k: KeyEvent, typing: bool) -> Option<WorktreeAction> {
        // A removal already with the host cannot be called back: the host is
        // taking the directory apart and its answer is the only thing left to
        // wait for. Offering Cancel or a way back here would let the user
        // leave the page believing the removal stopped, and then find the
        // tree gone. Only the scroll keys stay live, so a long preview can
        // still be read while the host works.
        if self.draft.borrow().removing() {
            return match k.code {
                KeyCode::Up => Some(WorktreeAction::Scroll(-1)),
                KeyCode::Down => Some(WorktreeAction::Scroll(1)),
                _ => None,
            };
        }
        Some(match k.code {
            KeyCode::Up => WorktreeAction::Scroll(-1),
            KeyCode::Down => WorktreeAction::Scroll(1),
            KeyCode::Left | KeyCode::Right | KeyCode::Tab if !typing => WorktreeAction::ToggleFocus,
            KeyCode::Enter if !typing => WorktreeAction::Confirm,
            KeyCode::Char('b') if bare(k) && !typing => WorktreeAction::ToggleBranch,
            KeyCode::Char('r') if bare(k) && !typing => WorktreeAction::Reread,
            KeyCode::Esc => WorktreeAction::Back,
            _ => return None,
        })
    }
}

/// Whether the key carries no modifier. The pane's letter actions are bare
/// keys, so a control-modified letter stays with the editor.
fn bare(k: KeyEvent) -> bool {
    k.modifiers.is_empty()
}

/// Handle a key when the /worktrees pane is active. Returns true if the key
/// was consumed (the caller should not fall through to generic input).
pub fn handle(app: &mut App, k: KeyEvent) -> bool {
    let typing = !app.ui.input.editor.is_empty();
    let Some(action) = app.ui.worktree_pane.action_for(k, typing) else {
        return false;
    };
    match action {
        WorktreeAction::Select(delta) => app.move_worktree_cursor(delta),
        WorktreeAction::Scroll(delta) => {
            let pane = &app.ui.worktree_pane;
            let rows = i32::from(pane.scroll.get()).saturating_add(delta);
            // The renderer holds the position at the last row that still fills
            // the block, so moving past the end stays here until the next way
            // up; the content is not known at the key.
            pane.scroll
                .set(u16::try_from(rows.max(0)).unwrap_or(u16::MAX));
        }
        WorktreeAction::OpenDetail => app.open_worktree_detail(),
        WorktreeAction::BeginRemoval => app.begin_worktree_removal(),
        WorktreeAction::EnterWorktree => app.enter_worktree_at_cursor(),
        WorktreeAction::StartSearch => app.ui.worktree_pane.search.set(true),
        WorktreeAction::Search(c) => app.ui.worktree_pane.list.query.push(c),
        WorktreeAction::SearchBackspace => {
            app.ui.worktree_pane.list.query.pop();
        }
        WorktreeAction::FinishSearch => app.ui.worktree_pane.search.set(false),
        WorktreeAction::ClearSearch => {
            app.ui.worktree_pane.list.clear_query();
            app.ui.worktree_pane.search.set(false);
        }
        WorktreeAction::ToggleFocus => app.ui.worktree_pane.draft.borrow_mut().toggle_focus(),
        WorktreeAction::Confirm => {
            let focus = app.ui.worktree_pane.draft.borrow().focus();
            match focus {
                RemoveFocus::Cancel => app.close_worktree_removal(),
                RemoveFocus::Remove => app.confirm_worktree_removal(),
            }
        }
        WorktreeAction::ToggleBranch => app.toggle_worktree_branch_deletion(),
        WorktreeAction::Reread => app.retry_worktree_preview(),
        WorktreeAction::Back => match app.ui.worktree_pane.level.get() {
            WorktreeLevel::Confirm => app.close_worktree_removal(),
            WorktreeLevel::Detail => app.ui.worktree_pane.set_level(WorktreeLevel::List),
            WorktreeLevel::List => {
                app.ui.worktree_pane.reset();
                app.ui.navigation.pane = Pane::Transcript;
            }
        },
    }
    true
}

#[cfg(test)]
mod tests;
