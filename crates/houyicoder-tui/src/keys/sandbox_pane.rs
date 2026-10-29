//! /sandbox pane key handling. The pane's own tab and level decide what a key
//! means; this module applies that action to the session. Every screen names
//! its keys in the row at the bottom, so a letter that acts on one screen is
//! not assumed to act the same way on the next.

use crossterm::event::{KeyCode, KeyEvent};

use crate::state::App;
use crate::ui::{CheckpointLevel, RestoreFocus, SandboxTab};

/// What a key asks the /sandbox pane to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SandboxAction {
    /// Move the listing cursor by this many rows.
    Select(i32),
    /// Move the block one level down by this many rows.
    Scroll(i32),
    /// Move to the other tab, in this direction.
    SwitchTab(i32),
    /// Open the detail of the row under the cursor.
    OpenDetail,
    /// Open the restore confirmation for the row under the cursor.
    BeginRestore,
    /// Move the confirmation's focus to the other button.
    ToggleFocus,
    /// Act on the focused button of the confirmation.
    Confirm,
    /// Return one level, or close the pane at the listing.
    Back,
}

impl App {
    /// The action a key asks for on the screen the pane is showing, or None
    /// when the key is not this pane's. typing reports that the input box
    /// holds text, which keeps bare letters for the message being written.
    fn sandbox_action(&self, k: KeyEvent, typing: bool) -> Option<SandboxAction> {
        let pane = &self.ui.sandbox_pane;
        if pane.tab.get() == SandboxTab::Sandbox {
            return Some(match k.code {
                KeyCode::Up => SandboxAction::Scroll(-1),
                KeyCode::Down => SandboxAction::Scroll(1),
                KeyCode::Left => SandboxAction::SwitchTab(-1),
                KeyCode::Right | KeyCode::Tab => SandboxAction::SwitchTab(1),
                KeyCode::Esc => SandboxAction::Back,
                _ => return None,
            });
        }
        match pane.level.get() {
            CheckpointLevel::List => Some(match k.code {
                KeyCode::Up => SandboxAction::Select(-1),
                KeyCode::Down => SandboxAction::Select(1),
                KeyCode::Enter if !typing => SandboxAction::OpenDetail,
                KeyCode::Char('r') if bare(k) && !typing => SandboxAction::BeginRestore,
                KeyCode::Left => SandboxAction::SwitchTab(-1),
                KeyCode::Right | KeyCode::Tab => SandboxAction::SwitchTab(1),
                KeyCode::Esc => SandboxAction::Back,
                _ => return None,
            }),
            CheckpointLevel::Detail => Some(match k.code {
                KeyCode::Up => SandboxAction::Scroll(-1),
                KeyCode::Down => SandboxAction::Scroll(1),
                KeyCode::Char('r') if bare(k) && !typing => SandboxAction::BeginRestore,
                KeyCode::Esc => SandboxAction::Back,
                _ => return None,
            }),
            CheckpointLevel::Restore => Some(match k.code {
                KeyCode::Up => SandboxAction::Scroll(-1),
                KeyCode::Down => SandboxAction::Scroll(1),
                KeyCode::Left | KeyCode::Right | KeyCode::Tab if !typing => {
                    SandboxAction::ToggleFocus
                }
                KeyCode::Enter if !typing => SandboxAction::Confirm,
                KeyCode::Esc => SandboxAction::Back,
                _ => return None,
            }),
        }
    }
}

/// Whether the key carries no modifier. The pane's letter actions are bare
/// keys, so a control-modified letter stays with the editor.
fn bare(k: KeyEvent) -> bool {
    k.modifiers.is_empty()
}

/// Handle a key when the /sandbox pane is active. Returns true if the key was
/// consumed (the caller should not fall through to generic input).
pub fn handle(app: &mut App, k: KeyEvent) -> bool {
    let typing = !app.ui.input.editor.is_empty();
    let Some(action) = app.sandbox_action(k, typing) else {
        return false;
    };
    match action {
        SandboxAction::Select(delta) => app.move_checkpoint_cursor(delta),
        SandboxAction::Scroll(delta) => app.ui.sandbox_pane.scroll_by(delta),
        SandboxAction::SwitchTab(delta) => app.switch_sandbox_tab(delta),
        SandboxAction::OpenDetail => app.open_checkpoint_detail(),
        SandboxAction::BeginRestore => app.begin_checkpoint_restore(),
        SandboxAction::ToggleFocus => app.ui.sandbox_pane.toggle_focus(),
        SandboxAction::Confirm => match app.ui.sandbox_pane.focus.get() {
            RestoreFocus::Cancel => app.close_checkpoint_restore(),
            RestoreFocus::Restore => app.confirm_checkpoint_restore(),
        },
        SandboxAction::Back => app.sandbox_back(),
    }
    true
}

#[cfg(test)]
mod tests;
