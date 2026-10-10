//! Where the frontend is looking: the screen, the open pane, the two
//! viewport modes, and the overlay pickers.

use crate::palette::PaletteState;
use crate::resume_picker::SessionPickerState;
use crate::skill_picker_state::SkillPickerState;
use crate::state::enums::{Pane, Screen, ViewportMode};
use houyicoder_protocol::frontend::LoginMode;

/// The user's position in the frontend: which screen and pane are showing,
/// the viewport mode with its pre-Scroll partner, and the overlay pickers
/// that cover the pane while open. A session switch resets these together;
/// the clipboard and terminal focus are process resources and stay out.
pub struct Navigation {
    /// The top-level screen currently showing.
    pub screen: Screen,
    /// Which content pane is open; Transcript is the base view.
    pub pane: Pane,
    /// The active viewport mode (Working / Focus / Scroll). Drives the layout
    /// in view::working: how many rows of chrome surround the content.
    pub viewport: ViewportMode,
    /// The viewport the user was in before entering Scroll, so Esc/End
    /// returns to it rather than re-deriving from stage (which would lose a
    /// manual Focus->Working fold).
    pub prev_viewport: ViewportMode,
    /// The login mode resolved at startup, when the host reported one.
    pub login_mode: Option<LoginMode>,
    /// The session picker overlay state (opened by /resume with no arg).
    pub resume_picker: SessionPickerState,
    /// The slash-command palette overlay state (query and list cursor).
    pub palette: PaletteState,
    /// The @ skill-picker overlay state: the open flag and its list cursor.
    /// Opening resets the cursor to zero, so the two move as one value.
    pub(crate) skill_picker: SkillPickerState,
}
