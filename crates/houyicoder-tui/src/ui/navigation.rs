//! Where the frontend is looking: the screen, the open pane, the two
//! viewport modes, and the overlay pickers.

use crate::input::InputField;
use crate::palette::PaletteState;
use crate::resume_picker::SessionPickerState;
use crate::skill_picker_state::SkillPickerState;
use crate::state::enums::{Pane, Screen, StatusTab, ViewportMode};
use houyicoder_protocol::frontend::LoginMode;

/// The /status pane's active sub-tab and inline session-name editor.
#[derive(Default)]
pub struct StatusPaneState {
    /// The active /status sub-tab (Status / Config / Usage). Tab or
    /// Left/Right cycles it; the pane header renders the three titles with
    /// the active one highlighted.
    pub tab: StatusTab,
    /// In-place session-name edit buffer for the status Status tab. None
    /// unless the user pressed e on the Status tab to rename; the pane
    /// renders the name row as an editable input + the keys route char,
    /// backspace, Left/Right to the buffer. Enter commits (sends a
    /// RenameSession request), Esc cancels.
    pub name_edit: Option<InputField>,
}

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
    /// The /status pane's tab selection and inline name editor.
    pub status_pane: StatusPaneState,
}

impl Navigation {
    /// Reset the session-local fields a switch must clear: the pane back to
    /// Transcript, the two viewport modes back to Working, and the overlay
    /// pickers plus the status-tab edit buffer to their defaults. The screen
    /// and login mode are applied by the caller (the landing screen and login
    /// mode come from apply_frontend_startup), so they stay out of the reset.
    pub(crate) fn reset_for_session_switch(&mut self) {
        self.pane = Pane::Transcript;
        self.viewport = ViewportMode::Working;
        self.prev_viewport = ViewportMode::Working;
        self.resume_picker = Default::default();
        self.palette = Default::default();
        self.skill_picker = Default::default();
        self.status_pane = Default::default();
    }
}
