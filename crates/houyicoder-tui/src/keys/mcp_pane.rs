//! /mcp pane key handling, on the sibling panes' grammar: Up/Down select on
//! the list and scroll the detail, Enter opens the detail, r reconnects, t
//! toggles, and Esc returns one level before it closes. The letter keys are
//! bare and only on an empty input box, so a message the user is writing
//! keeps its letters.

use crossterm::event::{KeyCode, KeyEvent};

use crate::mcp_pane_state::McpPaneState;
use crate::state::App;
use crate::state::enums::Pane;

/// What a key asks the /mcp pane to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpAction {
    /// Move the list cursor by this many rows.
    Select(i32),
    /// Scroll the detail body by this many rows.
    Scroll(i32),
    /// Open the detail for the row under the cursor.
    OpenDetail,
    /// Ask the selected server for a fresh handshake.
    Reconnect,
    /// Flip the selected server between enabled and disabled.
    Toggle,
    /// Return one level, or close the pane at the list.
    Back,
}

impl McpPaneState {
    /// The action a key asks for at the current level, or None when the key
    /// is not this pane's. typing reports that the input box holds text,
    /// which keeps bare letters for the message the user is writing. The
    /// action keys consult the selected row's gating, so a locked row offers
    /// neither and a disabled one offers no reconnect.
    fn action_for(&self, k: KeyEvent, typing: bool) -> Option<McpAction> {
        if self.level == 0 {
            Some(match k.code {
                KeyCode::Up => McpAction::Select(-1),
                KeyCode::Down => McpAction::Select(1),
                KeyCode::Enter if !typing => McpAction::OpenDetail,
                KeyCode::Char('r') if bare(k) && !typing && self.allows_reconnect() => {
                    McpAction::Reconnect
                }
                KeyCode::Char('t') if bare(k) && !typing && self.allows_toggle() => {
                    McpAction::Toggle
                }
                KeyCode::Esc => McpAction::Back,
                _ => return None,
            })
        } else {
            Some(match k.code {
                KeyCode::Up => McpAction::Scroll(-1),
                KeyCode::Down => McpAction::Scroll(1),
                KeyCode::PageUp => McpAction::Scroll(-10),
                KeyCode::PageDown => McpAction::Scroll(10),
                KeyCode::Char('r') if bare(k) && !typing && self.allows_reconnect() => {
                    McpAction::Reconnect
                }
                KeyCode::Char('t') if bare(k) && !typing && self.allows_toggle() => {
                    McpAction::Toggle
                }
                KeyCode::Esc => McpAction::Back,
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

/// Handle a key when the /mcp pane is active. Returns true if the key was
/// consumed, so the caller does not fall through to generic input.
pub(crate) fn handle(app: &mut App, k: KeyEvent) -> bool {
    let typing = !app.ui.input.editor.is_empty();
    let Some(action) = app.ui.mcp_pane.action_for(k, typing) else {
        return false;
    };
    match action {
        McpAction::Select(delta) => app.ui.mcp_pane.move_cursor(delta),
        McpAction::Scroll(delta) => app.ui.mcp_pane.scroll_detail(delta),
        McpAction::OpenDetail => app.ui.mcp_pane.open_detail(),
        McpAction::Reconnect => app.reconnect_mcp_at_cursor(),
        McpAction::Toggle => app.toggle_mcp_at_cursor(),
        McpAction::Back => {
            if app.ui.mcp_pane.level > 0 {
                app.ui.mcp_pane.close_detail();
            } else {
                app.ui.mcp_pane.reset();
                app.ui.navigation.pane = Pane::Transcript;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests;
