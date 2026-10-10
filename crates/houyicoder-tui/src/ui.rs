//! The frontend's presentation and interaction state.
//!
//! Ui owns where the user is looking, what the user has typed but not sent,
//! what the pointer has selected, and the transient notices and clipboard
//! port the interaction loop reads. It holds no session data, no connection
//! handles, and no second copy of server-returned state; those live on the
//! session, and views read them through App.

mod navigation;

use std::sync::Arc;

use crate::input::Input;
use crate::notifications::NotificationState;
use crate::selection::ClipboardWriter;
use crate::selections::Selections;
use crate::state::QueueViewState;

pub use crate::ui::navigation::Navigation;

/// Presentation and interaction state for the visible frontend: navigation,
/// input, selection, notifications, clipboard, focus, and the queued-input
/// hit-test geometry.
pub struct Ui {
    /// Where the user is looking: the screen, the pane, the two viewport
    /// modes, and the overlay pickers.
    pub navigation: Navigation,
    /// The input subsystem: the chat editor, the prompt-history navigation,
    /// and the two paste halves, held as one value so a session switch resets
    /// them together.
    pub(crate) input: Input,
    /// Selection and hit-test state for the queued-input interface. The
    /// queue's own cursor lives on the InputQueue; this holds only the
    /// last-rendered strip rectangle the pointer handler reads back.
    pub queue_view: QueueViewState,
    /// The four per-region drag selections: the transcript's main selection
    /// and the status, pane, and approval selections, each in its own
    /// coordinate space so a drag on one region never collides with another.
    pub(crate) selection: Selections,
    /// Transient notification toast: one-line auto-expiring hint above the
    /// input box (copy feedback, exit-again prompt). Poll-driven expiry.
    pub notifications: NotificationState,
    /// Pluggable clipboard writer. Production holds a SystemClipboard
    /// (pbcopy/OSC 52); adversarial selection tests inject a
    /// RecordingClipboard so the exact copied text can be asserted without
    /// touching the OS clipboard. Arc<dyn> so the frontend state stays
    /// Send + Sync across the TUI/runner.
    pub clipboard: Arc<dyn ClipboardWriter>,
    /// Whether the terminal window has focus (FocusGained/FocusLost events).
    /// The input cursor (invert) gates on this so the caret hides when the
    /// window is unfocused. Defaults true (assume focused at startup).
    pub terminal_focused: bool,
}
