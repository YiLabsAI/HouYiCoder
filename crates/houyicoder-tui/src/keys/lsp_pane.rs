//! /lsp pane key handling, on the sibling panes' grammar: Up/Down select on
//! the list and scroll the detail, Enter opens the detail, r restarts, t
//! toggles, and Esc returns one level before it closes. The letter keys are
//! bare and only on an empty input box, so a message the user is writing
//! keeps its letters.

use crossterm::event::{KeyCode, KeyEvent};

use crate::lsp_pane_state::LspPaneState;
use crate::state::App;
use crate::state::enums::Pane;

/// What a key asks the /lsp pane to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LspAction {
    /// Move the list cursor by this many rows.
    Select(i32),
    /// Scroll the detail body by this many rows.
    Scroll(i32),
    /// Open the detail for the row under the cursor.
    OpenDetail,
    /// Stop the selected server and start it again.
    Restart,
    /// Flip the selected server between enabled and disabled.
    Toggle,
    /// Return one level, or close the pane at the list.
    Back,
}

impl LspPaneState {
    /// The action a key asks for at the current level, or None when the key
    /// is not this pane's. typing reports that the input box holds text,
    /// which keeps bare letters for the message the user is writing. The
    /// action keys consult the selected row's gating, so a locked row offers
    /// no toggle and a disabled one offers no restart.
    fn action_for(&self, k: KeyEvent, typing: bool) -> Option<LspAction> {
        if self.level == 0 {
            Some(match k.code {
                KeyCode::Up => LspAction::Select(-1),
                KeyCode::Down => LspAction::Select(1),
                KeyCode::Enter if !typing => LspAction::OpenDetail,
                KeyCode::Char('r') if bare(k) && !typing && self.allows_restart() => {
                    LspAction::Restart
                }
                KeyCode::Char('t') if bare(k) && !typing && self.allows_toggle() => {
                    LspAction::Toggle
                }
                KeyCode::Esc => LspAction::Back,
                _ => return None,
            })
        } else {
            Some(match k.code {
                KeyCode::Up => LspAction::Scroll(-1),
                KeyCode::Down => LspAction::Scroll(1),
                KeyCode::PageUp => LspAction::Scroll(-10),
                KeyCode::PageDown => LspAction::Scroll(10),
                KeyCode::Char('r') if bare(k) && !typing && self.allows_restart() => {
                    LspAction::Restart
                }
                KeyCode::Char('t') if bare(k) && !typing && self.allows_toggle() => {
                    LspAction::Toggle
                }
                KeyCode::Esc => LspAction::Back,
                _ => return None,
            })
        }
    }
}

/// Whether the key carries no modifier. The pane's letter actions are bare
/// keys, so a control-modified letter stays with the editor.
fn bare(k: KeyEvent) -> bool {
    k.modifiers.is_empty()
}

/// Handle a key when the /lsp pane is active. Returns true if the key was
/// consumed, so the caller does not fall through to generic input.
pub(crate) fn handle(app: &mut App, k: KeyEvent) -> bool {
    let typing = !app.ui.input.editor.is_empty();
    let Some(action) = app.ui.lsp_pane.action_for(k, typing) else {
        return false;
    };
    match action {
        LspAction::Select(delta) => app.ui.lsp_pane.move_cursor(delta),
        LspAction::Scroll(delta) => app.ui.lsp_pane.scroll_detail(delta),
        LspAction::OpenDetail => app.ui.lsp_pane.open_detail(),
        LspAction::Restart => app.restart_lsp_at_cursor(),
        LspAction::Toggle => app.toggle_lsp_at_cursor(),
        LspAction::Back => {
            if app.ui.lsp_pane.level > 0 {
                app.ui.lsp_pane.close_detail();
            } else {
                app.ui.lsp_pane.reset();
                app.ui.navigation.pane = Pane::Transcript;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests;
