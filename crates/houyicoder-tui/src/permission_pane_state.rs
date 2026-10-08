//! The state of the permission pane: which tab is open (the rule filter or the
//! recent verdict log), the cursor row inside it, the typed-input sub-mode
//! (add / remove / search / none), and the pane-local search buffer. One owner
//! because the four move together on every tab switch and every input-mode
//! transition: switching tab resets the cursor, entering search clears the
//! cursor and the buffer, and the render path reads the tab, cursor, input, and
//! buffer as one set to lay the list and the prompt out.

use crate::state::enums::{PermissionInput, PermissionTab};

/// The permission pane state. Fields are pub(crate) so the key handler and the
/// render path reach a field directly; the whole value is what a tab switch or
/// input-mode entry replaces, which is the contract the flat fields could only
/// carry as a comment.
#[derive(Debug)]
pub(crate) struct PermissionPaneState {
    /// The selected tab in the permission pane: the rule filter (Allow / Ask /
    /// Deny / Workspace) or the recent verdict log.
    pub(crate) tab: PermissionTab,
    /// The cursor row in the current tab; clamped at render against the live
    /// list length, so it never points past the last row.
    pub(crate) cursor: usize,
    /// The active typed-input sub-mode (add / remove / search), or None for
    /// plain list navigation. None re-purposes the main input box back to the
    /// slash-command buffer.
    pub(crate) input: PermissionInput,
    /// The pane-local search buffer. Decoupled from the main input so a search
    /// filter never eats a slash command's leading slash.
    pub(crate) search: String,
}

impl Default for PermissionPaneState {
    fn default() -> Self {
        Self {
            tab: PermissionTab::Allow,
            cursor: 0,
            input: PermissionInput::None,
            search: String::new(),
        }
    }
}
