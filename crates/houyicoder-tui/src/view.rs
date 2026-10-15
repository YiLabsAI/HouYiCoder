//! View module: pure rendering of App state. No mutation, no I/O. Each screen
//! and overlay has its own submodule so files stay small.

pub(crate) mod fleet;
pub(crate) mod list_pane_state;

pub mod agents_pane;
pub mod approval;
pub mod artifact;
pub mod ask_question;
pub mod capability;
pub mod components;
pub mod console;
pub mod context_view;
pub mod export_log;
pub mod hooks_pane;
pub mod input_bar;
pub mod login;
pub mod logo;
pub mod markers;
pub mod memory_pane;
pub mod model_pane;
pub mod navigation;
pub mod palette;
pub mod pane;
pub mod queue_overlay;
pub mod queue_pane;
pub mod relative_time;
pub mod resume_picker;
pub mod skill_picker;
pub mod skills_pane;
pub mod spinner;
pub mod status;
pub mod teammate_view;
pub mod toast;
pub mod todo_list;
pub mod trajectory_pane;
pub mod trust;
pub mod word_diff;
pub mod working;
pub mod worktree_pane;

use ratatui::{Frame, layout::Rect, style::Color};

use crate::state::{App, Screen};

/// Map an agent badge color name to a foreground color. Unknown names fall
/// back to None so a misspelled color renders the default foreground rather
/// than a wrong hue. Shared by the teammate banner + the inline fold-group
/// summary so both read one source.
pub(crate) fn badge_color(name: &str) -> Option<Color> {
    Some(match name {
        "red" => Color::Red,
        "green" => Color::Green,
        "blue" => Color::Blue,
        "yellow" => Color::Yellow,
        "cyan" => Color::Cyan,
        "magenta" => Color::Magenta,
        _ => return None,
    })
}

/// Top-level draw: pick a screen. The slash palette is rendered inline as
/// part of the working-surface layout (see working::draw), not as a floating
/// overlay, so opening it pushes the transcript up rather than covering it.
/// The approval card is likewise inline at the transcript tail (see
/// working::draw_transcript), not a floating popup.
pub fn draw(f: &mut Frame, app: &App) {
    reset_click_regions(app);
    // The startup workspace-trust banner is the sole pre-chat setup screen
    // while a trust ask is pending — the main view does not mount until the
    // user answers. A top banner, not a centered popup over a live chat.
    if app.pending_trust().is_some() {
        trust::draw(f, app);
        return;
    }
    match app.ui.navigation.screen {
        Screen::Login => login::draw(f, app),
        Screen::Console => console::draw(f, app),
        Screen::Working => working::draw(f, app),
    }
}

/// Zero every click-target rectangle at the top of a frame. Each region
/// republishes its rect while it draws, so a region this frame does not draw
/// keeps the zero and cannot be reached through the previous frame's
/// geometry. The rule lives here rather than beside each region because the
/// frames that skip a region are not in one place: the login, console, and
/// trust screens draw no transcript at all, a fullscreen pane such as the
/// artifact view draws none either, and the viewport variants differ in
/// which chrome they publish. Left stale, a rect from the last frame routes
/// a click or drag at content that is no longer on screen.
///
/// Called by the top-level draw and again by each working-mode variant, so a
/// caller that reaches a variant directly gets the same guarantee.
pub(crate) fn reset_click_regions(app: &App) {
    let zero = Rect::new(0, 0, 0, 0);
    let layout = &app.ui.frame.layout;
    layout.transcript_rect.set(zero);
    layout.jump_to_bottom_rect.set(zero);
    layout.pane_rect.set(zero);
    layout.status_rect.set(zero);
    layout.approval_rect.set(zero);
    layout.fleet_rect.set(zero);
    layout.queue_rect.set(zero);
}

pub mod line_wrap;

#[cfg(test)]
mod tests;
